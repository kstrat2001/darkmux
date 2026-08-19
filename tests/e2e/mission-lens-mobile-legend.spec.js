// #1868 packet 3 fix — the mobile tap-to-open status-legend popover
// (`.legbtn`/`.legendpop`, mission-graph.html:299-302,379-381,432,448,
// 2119-2122). The port carried `.legend { display: none }` at the ~700px
// breakpoint (correct — desktop's inline dots don't fit a phone header) but
// dropped the REPLACEMENT affordance legacy added for that same breakpoint,
// leaving phones — the timeline renderer's primary audience per #1404 —
// with zero way to see the status legend at all. This spec proves the
// popover exists, opens/closes on tap, and that the dots it shows are
// unreachable any other way at this width.
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'mobile-legend-mission';

function routeFlowNoise(page) {
  return Promise.all([
    page.route(/\/flow\/\d{4}-\d{2}-\d{2}(\?.*)?$/, (r) =>
      r.fulfill({ contentType: 'application/json', body: '[]' })
    ),
    page.route(/\/flow\/\d{4}-\d{2}-\d{2}\/stream(\?.*)?$/, (r) =>
      r.fulfill({ contentType: 'text/event-stream', body: '' })
    ),
    page.route(/\/flow-mission\/[^/?]+(\?.*)?$/, (r) =>
      r.fulfill({ contentType: 'application/json', body: JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 }) })
    ),
  ]);
}

async function gotoMobileMission(page) {
  await page.setViewportSize({ width: 390, height: 844 });
  await routeFlowNoise(page);
  const graph = {
    mission_id: MISSION_ID,
    mission_status: 'active',
    nodes: [{ id: 'phase-a', kind: 'phase', label: 'Phase A', status: 'running', depth: 0, steps: [] }],
    edges: [],
    generated_at_ms: 0,
  };
  await page.route(`**/mission/${MISSION_ID}/graph.json`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(graph) })
  );
  await page.goto(`/index-live.html#mission=${MISSION_ID}`);
  await expect(page.locator('.missionlens .midname')).toHaveText(MISSION_ID);
}

test('at mobile width, .legend is hidden but .legbtn is a real replacement affordance', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await gotoMobileMission(page);

  await expect(page.locator('.missionlens .legend')).not.toBeVisible();
  const legbtn = page.locator('.missionlens .legbtn');
  await expect(legbtn).toBeVisible();
  await expect(legbtn).toHaveText(/legend/i);

  expect(pageErrors).toEqual([]);
});

test('tapping .legbtn opens the popover with the status dots; tapping again closes it', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await gotoMobileMission(page);

  const legbtn = page.locator('.missionlens .legbtn');
  const pop = page.locator('.missionlens .legendpop');

  // Not reachable before the tap — this is the exact gap the fix closes.
  await expect(pop).toHaveCount(0);

  await legbtn.click();
  await expect(pop).toBeVisible();
  await expect(pop).toHaveClass(/\bon\b/);
  await expect(pop).toContainText('running');
  await expect(pop).toContainText('complete');
  await expect(pop).toContainText('error');

  await legbtn.click();
  await expect(pop).toHaveCount(0);

  expect(pageErrors).toEqual([]);
});

test('the legend button is keyboard-reachable and activates on Enter, same as every other header control', async ({ page }) => {
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  await gotoMobileMission(page);

  const legbtn = page.locator('.missionlens .legbtn');
  await legbtn.focus();
  await expect(legbtn).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page.locator('.missionlens .legendpop')).toBeVisible();

  expect(pageErrors).toEqual([]);
});
