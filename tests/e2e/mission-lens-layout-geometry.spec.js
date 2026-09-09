// #2057 / #2058 — geometry the mission graph lens must hold in a real browser.
// jsdom cannot measure, so these live here. Each one was red before its fix:
// sibling tasks with two step rows overlapped (the layout pitch described a
// smaller card than the CSS draws), phase→phase edges ran diagonally between
// left-anchored bands of different widths, and the canvas outgrew the
// viewport so React Flow's controls and minimap sat below the fold.
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'geometry';
const LABEL_MISSION_ID = 'geometry-label';

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
  await page.route(`**/mission/${MISSION_ID}/graph.json`, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify(graphSnapshot()) }));
  await page.route(/\/flow\/[^/]+\/mission\/.*/, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) }));
  await page.route(/\/flow\/[^/]+\/backfill.*/, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route(/\/flow\/[^/]+\/stream.*/, (r) => r.fulfill({ status: 204, body: '' }));
  await page.route(/\/(missions|phases|runs|lab\/runs|machine\/.*|presence.*|fleet\/.*)(\?.*)?$/, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
}

function intersects(a, b) {
  return a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;
}

async function taskBoxes(page) {
  const nodes = page.locator('.missionlens .mnode.k-task');
  await expect(nodes).toHaveCount(8);
  const boxes = [];
  for (let i = 0; i < 8; i++) boxes.push({ label: await nodes.nth(i).innerText(), box: await nodes.nth(i).boundingBox() });
  return boxes;
}

test.describe('mission graph geometry', () => {
  test('sibling tasks with two step rows never overlap (#2057)', async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await routeAll(page);
    await page.goto(`/index-live.html#mission=${MISSION_ID}`);
    const boxes = await taskBoxes(page);
    for (let i = 0; i < boxes.length; i++) {
      for (let j = i + 1; j < boxes.length; j++) {
        expect(intersects(boxes[i].box, boxes[j].box), `${boxes[i].label.split('\n')[1]} overlaps ${boxes[j].label.split('\n')[1]}`).toBe(false);
      }
    }
  });

  test('phase bands share one width so phase→phase edges are vertical (#2057)', async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 720 });
    await routeAll(page);
    await page.goto(`/index-live.html#mission=${MISSION_ID}`);
    const bands = page.locator('.missionlens .phasegroup');
    await expect(bands).toHaveCount(3);
    const widths = [];
    const centers = [];
    for (let i = 0; i < 3; i++) {
      const b = await bands.nth(i).boundingBox();
      widths.push(Math.round(b.width));
      centers.push(Math.round(b.x + b.width / 2));
    }
    expect(new Set(widths).size, `band widths differ: ${widths.join(', ')}`).toBe(1);
    expect(Math.max(...centers) - Math.min(...centers), `band centers differ: ${centers.join(', ')}`).toBeLessThanOrEqual(2);
  });

  for (const vp of [{ width: 1280, height: 720 }, { width: 390, height: 844 }]) {
    test(`zoom controls and minimap stay inside a ${vp.width}x${vp.height} viewport (#2058)`, async ({ page }) => {
      await page.setViewportSize(vp);
      await routeAll(page);
      await page.goto(`/index-live.html#mission=${MISSION_ID}`);
      // Narrow viewports default to the list renderer; the controls only exist on the canvas.
      // Wait for the lens to have PICKED a renderer before asking which one it
      // picked. Without this the guard races first paint: `.canvas` is also
      // absent while the lens is still mounting, so on a slow run the click
      // fires on a wide viewport and switches the canvas default INTO the list
      // renderer -- after which `.mnode` never exists and this test fails
      // ~1 run in 3, for reasons unrelated to geometry.
      await expect(page.locator('.missionlens')).toBeVisible();
      await page.waitForFunction(() => !!document.querySelector('.missionlens .canvas, .missionlens .tlt-hd'));
      if ((await page.locator('.missionlens .canvas').count()) === 0) await page.locator('button[title="switch renderer"]').click();
      await expect(page.locator('.missionlens .mnode').first()).toBeVisible();
      const controls = await page.locator('.react-flow__controls').boundingBox();
      expect(controls, 'controls rendered').not.toBeNull();
      expect(controls.y + controls.height, 'controls bottom inside viewport').toBeLessThanOrEqual(vp.height + 0.5);
      expect(controls.x, 'controls left inside viewport').toBeGreaterThanOrEqual(-0.5);
      const minimap = page.locator('.react-flow__minimap');
      if (await minimap.count()) {
        const m = await minimap.boundingBox();
        expect(m.x + m.width, 'minimap right inside viewport').toBeLessThanOrEqual(vp.width + 0.5);
        expect(m.y + m.height, 'minimap bottom inside viewport').toBeLessThanOrEqual(vp.height + 0.5);
      }
      const overflow = await page.evaluate(() => {
        const top = document.querySelector('.missionlens .top');
        return top ? top.scrollWidth - top.clientWidth : 0;
      });
      expect(overflow, 'lens header does not overflow horizontally').toBeLessThanOrEqual(0);
    });
  }

  // (#2406, post-review round 2) The phase label block is `position:absolute`
  // inside the band, so nothing bounds its width by default — it is
  // shrink-to-fit against an infinite available width. That was invisible
  // while the block read `PHASE` + a short name; adding the status chip and
  // the counts to the same line made it real. Measured on this fixture before
  // the fix: `investigate` + a three-part note ran 85px past the band's right
  // edge, `adjudicate` + a five-part note 309px past — clipped mid-word by
  // the events pane, so the counts the fix exists to expose were unreadable.
  //
  // The fix has two parts and this test is red without either. `max-width` on
  // `.pg-label` bounds the absolute block to the band (drop it: the label runs
  // 85px past again). `.pg-note`'s large shrink factor decides which item pays
  // for that bound (drop it back to an equal factor: the name and the counts
  // ellipsize together and the band reads `invest…`, which is worse than the
  // overflow was — the band loses its identity to make room for a truncated
  // detail). Both items already ellipsize on their own; `overflow: hidden` is
  // what zeroes a flex item's automatic minimum size, so nothing here needs an
  // explicit `min-width: 0`.
  //
  // Tasks here carry NO steps on purpose: `taskWidth` then returns `COL_W`
  // (260) rather than `TASK_W_WITH_STEPS` (360), which is the narrow band a
  // real short-phase mission draws and the case where even the SHORT note
  // escapes.
  function labelSnapshot() {
    return {
      mission_id: LABEL_MISSION_ID,
      mission_status: 'active',
      nodes: [
        {
          id: 'pa', kind: 'phase', label: 'investigate', status: 'degraded', depth: 0, steps: [],
          statusNote: '7 complete · 1 errored · 4 abandoned',
        },
        { id: 'ta', kind: 'task', label: 'Probe', parentId: 'pa', status: 'complete', depth: 0, steps: [] },
        {
          id: 'pb', kind: 'phase', label: 'adjudicate', status: 'running', depth: 1, steps: [],
          statusNote: '2 complete · 1 errored · 3 abandoned · 5 running · 9 planned',
        },
        { id: 'tb', kind: 'task', label: 'Judge', parentId: 'pb', status: 'running', depth: 0, steps: [] },
      ],
      edges: [],
      generated_at_ms: 0,
    };
  }

  test('the phase label never escapes its own band, however long the counts run (#2406)', async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.route(`**/mission/${LABEL_MISSION_ID}/graph.json*`, (r) =>
      r.fulfill({ contentType: 'application/json', body: JSON.stringify(labelSnapshot()) }));
    await page.route(/\/flow-mission\/.*/, (r) => r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) }));
    await routeAll(page);
    await page.goto(`/index-live.html#mission=${LABEL_MISSION_ID}`);

    const bands = page.locator('.missionlens .phasegroup');
    await expect(bands).toHaveCount(2);
    await expect(page.locator('.missionlens .mnode').first()).toBeVisible();

    for (let i = 0; i < 2; i++) {
      const band = bands.nth(i);
      const box = await band.boundingBox();
      const text = (await band.locator('.pg-label').innerText()).replace(/\n/g, ' / ');
      // Measure EVERY rendered piece, not just the label wrapper. Bounding the
      // wrapper alone is not enough and is the trap this loop exists to avoid:
      // an unshrinkable flex item overflows its bounded parent, so the wrapper
      // measures clean while the text that overflowed it is still on screen,
      // still outside the band, still clipped by the events pane.
      for (const sel of ['.pg-label', '.pg-state', '.pg-name', '.pg-note']) {
        const el = band.locator(sel);
        if (!(await el.count())) continue;
        const b = await el.boundingBox();
        const overhang = (b.x + b.width) - (box.x + box.width);
        expect(
          overhang,
          `${sel} runs ${overhang.toFixed(1)}px past its band's right edge (band ${Math.round(box.width)}px wide, label "${text}")`,
        ).toBeLessThanOrEqual(0.5);
        expect(b.x, `${sel} starts inside its band`).toBeGreaterThanOrEqual(box.x - 0.5);
      }
    }

    // The bound must not have been bought by hiding the counts entirely —
    // the whole point of the line is that they are readable text.
    await expect(bands.first().locator('.pg-note')).toBeVisible();

    // ...nor by ellipsizing the phase's NAME to make room for counts that are
    // themselves being ellipsized. The name is the band's identity; the counts
    // are the detail. When the line has to give, the counts give first.
    for (let i = 0; i < 2; i++) {
      const name = bands.nth(i).locator('.pg-name');
      const m = await name.evaluate((el) => ({ cw: el.clientWidth, sw: el.scrollWidth, t: el.textContent }));
      expect(m.sw, `the phase name "${m.t}" is ellipsized (${m.sw}px of text in ${m.cw}px)`).toBeLessThanOrEqual(m.cw + 1);
    }
  });
});
