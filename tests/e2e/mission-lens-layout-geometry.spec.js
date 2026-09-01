// #2057 / #2058 — geometry the mission graph lens must hold in a real browser.
// jsdom cannot measure, so these live here. Each one was red before its fix:
// sibling tasks with two step rows overlapped (the layout pitch described a
// smaller card than the CSS draws), phase→phase edges ran diagonally between
// left-anchored bands of different widths, and the canvas outgrew the
// viewport so React Flow's controls and minimap sat below the fold.
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'geometry';

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
});
