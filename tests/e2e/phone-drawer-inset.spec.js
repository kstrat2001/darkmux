// The phone drawer is LAYOUT, not an overlay — every lens's content ends
// where the collapsed drawer's tab bar begins (operator, 2026-09-05).
//
// The finding: on the mission graph lens the React Flow controls (+/- zoom,
// bottom-left) and the minimap (bottom-right) sat UNDER the "Machine info |
// Events" bar. `MissionCanvas.tsx`'s `fit()` (#2058) sizes the canvas to
// `window.innerHeight - top` — it already knew the canvas must not run past
// the fold, but it treated the raw viewport bottom as the content bottom and
// had no idea the last 58px belong to a fixed bar.
//
// Two different failures live here and only one is a bug:
//
//   * An ABSOLUTELY POSITIONED control pinned to its container's bottom edge
//     can never be scrolled out from under a fixed bar. That is the defect.
//   * A control in normal FLOW that happens to sit under the bar at one
//     scroll position (the machine lens's `.mm-odo-i` row at rest — C3) is
//     reachable by scrolling, and `.app-shell`'s own `padding-bottom`
//     already guarantees the document ends above the bar. That is not the
//     same problem, and this file asserts the two separately rather than
//     conflating them into one number.
//
// Every route below names a READY selector and waits for it before
// measuring. Without that this whole file is vacuous by construction: a
// route that rendered nothing has no pinned controls, and "no control is
// under the bar" passes loudest exactly when the lens failed to mount. An
// earlier draft pointed at demo-only mission and dispatch ids that do not
// exist in this harness, and those two cases went green against an empty
// stage.
const { test, expect } = require('@playwright/test');

const MISSION_ID = 'drawer-inset';

/** The smallest graph that still renders a canvas with phases, tasks and a
 * step row — this file is about where the canvas ENDS, not what it draws, so
 * the sibling geometry spec's fuller snapshot would be noise here. */
function graphSnapshot() {
  return {
    mission_id: MISSION_ID,
    mission_status: 'finalized',
    nodes: [
      { id: 'phase-investigate', kind: 'phase', label: 'Investigate', status: 'complete', depth: 0, steps: [] },
      {
        id: 'bundle', kind: 'task', label: 'Bundle', parentId: 'phase-investigate', status: 'complete', depth: 0,
        steps: [{ id: 'bundle-1', kind: 'review.bundle', label: 'Bundle', status: 'complete' }],
      },
      {
        id: 'probe', kind: 'task', label: 'Probe', parentId: 'phase-investigate', status: 'complete', depth: 1,
        steps: [{ id: 'probe-1', kind: 'dispatch.map', label: 'Dispatch (map)', status: 'complete', model: 'darkmux:qwen3.6-35b-a3b' }],
      },
      { id: 'phase-report', kind: 'phase', label: 'Report', status: 'complete', depth: 1, steps: [] },
      {
        id: 'synthesis', kind: 'task', label: 'Synthesis', parentId: 'phase-report', status: 'complete', depth: 0,
        steps: [{ id: 'synth-1', kind: 'review.synthesis', label: 'Synthesis', status: 'complete' }],
      },
    ],
    edges: [
      { id: 'e1', source: 'bundle', target: 'probe', kind: 'depends' },
      { id: 'p1', source: 'phase-investigate', target: 'phase-report', kind: 'phase' },
    ],
    generated_at_ms: 0,
  };
}

/** The daemon stubs `index-live.html` needs — the same set, and the same
 * reasons, as `mission-lens-layout-geometry.spec.js`'s own `routeAll`. */
async function routeMission(page) {
  await page.route(`**/mission/${MISSION_ID}/graph.json`, (r) =>
    r.fulfill({ contentType: 'application/json', body: JSON.stringify(graphSnapshot()) }),
  );
  const emptyBody = JSON.stringify({ records: [], count: 0, truncated: false, generated_at_ms: 0 });
  await page.route(/\/flow-mission\/.*/, (r) => r.fulfill({ contentType: 'application/json', body: emptyBody }));
  await page.route(/\/flow\/[^/]+\/mission\/.*/, (r) => r.fulfill({ contentType: 'application/json', body: emptyBody }));
  await page.route(/\/flow\/[^/]+\/backfill.*/, (r) => r.fulfill({ contentType: 'application/json', body: '[]' }));
  await page.route(/\/flow\/[^/]+\/stream.*/, (r) => r.fulfill({ status: 204, body: '' }));
  await page.route(/\/(missions|phases|runs|lab\/runs|machine\/.*|presence.*|fleet\/.*)(\?.*)?$/, (r) =>
    r.fulfill({ contentType: 'application/json', body: '[]' }),
  );
}

// `index.html` is the static XSS-fixture harness — a real committed flow
// file, so the plain lenses render real content. `index-lifecycle.html` is
// the same shape pointed at a fixture with clean session ids, which is where
// a dispatch drill-in is reachable without a daemon.
const ROUTES = [
  ['fleet', '/index.html#lens=fleet', '.savrow'],
  ['console', '/index.html#lens=console', '.panelwrap'],
  // `.stagehdr` is this suite's documented "a lens rendered" hook (see
  // `styles.css`'s own note on it); the machine lens has its own root.
  ['runs', '/index.html#lens=runs', '.stagehdr'],
  ['machine', '/index.html#lens=machine', '.machine-lens'],
  ['dispatch', '/index-lifecycle.html#dispatch=sess-clean-complete', '.session-run'],
];

test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

/** The collapsed drawer's own top edge — the line content must not cross. */
async function drawerTop(page) {
  return page.evaluate(() => {
    const d = document.querySelector('.phone-drawer');
    return d ? Math.round(d.getBoundingClientRect().top) : null;
  });
}

/** Every absolutely/fixed-positioned element inside the stage, with its box. */
async function pinnedControls(page) {
  return page.evaluate(() => {
    const stage = document.querySelector('.app-shell__stage');
    if (!stage) return [];
    return [...stage.querySelectorAll('*')]
      .filter((el) => {
        const p = getComputedStyle(el).position;
        if (p !== 'absolute' && p !== 'fixed') return false;
        const b = el.getBoundingClientRect();
        return b.width > 0 && b.height > 0;
      })
      .map((el) => {
        const b = el.getBoundingClientRect();
        return { sel: el.className.toString().split(' ')[0] || el.tagName, top: Math.round(b.top), bottom: Math.round(b.bottom) };
      });
  });
}

/** Offenders = pinned controls ON SCREEN and cut by the bar.
 *
 * INTERSECTING the bar at rest, not merely "below y=drawerTop": most pinned
 * boxes in these lenses (`.sbar` activity segments, `.mm-row-*` meter fills)
 * are absolutely positioned inside a NORMAL-FLOW container far down the
 * document, so they sit at y=1200+ on an 844px viewport and scroll into view
 * above the bar like any other content. */
async function offenders(page, top) {
  const vh = await page.evaluate(() => window.innerHeight);
  return (await pinnedControls(page)).filter((c) => c.top < vh && c.bottom > top);
}

test.describe('every lens ends above the collapsed phone drawer', () => {
  for (const [name, url, ready] of ROUTES) {
    test(`${name}: no pinned control crosses the drawer bar`, async ({ page }) => {
      await page.goto(url);
      await expect(
        page.locator(ready).first(),
        `${name} never rendered — the measurement below would be vacuous`,
      ).toBeVisible();
      const top = await drawerTop(page);
      expect(top, 'the phone drawer must be mounted at this viewport').not.toBeNull();
      const bad = await offenders(page, top);
      expect(bad, `pinned controls under the drawer bar (top=${top}): ${JSON.stringify(bad)}`).toEqual([]);
    });
  }

  test('mission TIMELINE (the phone default): no pinned control crosses the drawer bar', async ({ page }) => {
    await routeMission(page);
    await page.goto(`/index-live.html#mission=${MISSION_ID}`);
    await expect(page.locator('.missionlens')).toBeVisible();
    await page.waitForFunction(() => !!document.querySelector('.missionlens .canvas, .missionlens .tlt-hd'));
    const top = await drawerTop(page);
    const bad = await offenders(page, top);
    expect(bad, `pinned controls under the drawer bar (top=${top}): ${JSON.stringify(bad)}`).toEqual([]);
  });

  test('mission GRAPH view: the canvas and its React Flow overlays stop at the drawer', async ({ page }) => {
    await routeMission(page);
    await page.goto(`/index-live.html#mission=${MISSION_ID}`);

    // A phone defaults to the TIMELINE renderer; the canvas — and with it the
    // bottom-pinned controls this test is about — is one tap away. Waiting
    // for the lens to have PICKED a renderer before asking which one it
    // picked is the race guard `mission-lens-layout-geometry.spec.js`
    // documents: `.canvas` is also absent while the lens is still mounting,
    // so an unguarded click can toggle the wrong way.
    await expect(page.locator('.missionlens')).toBeVisible();
    await page.waitForFunction(() => !!document.querySelector('.missionlens .canvas, .missionlens .tlt-hd'));
    if ((await page.locator('.missionlens .canvas').count()) === 0) {
      await page.locator('button[title="switch renderer"]').click();
    }
    await expect(page.locator('.missionlens .mnode').first()).toBeVisible();
    await page.waitForSelector('.react-flow__controls');

    const top = await drawerTop(page);
    const boxes = await page.evaluate(() => {
      const r = (s) => {
        const e = document.querySelector(s);
        return e ? { top: Math.round(e.getBoundingClientRect().top), bottom: Math.round(e.getBoundingClientRect().bottom) } : null;
      };
      return { canvas: r('.missionlens .canvas'), controls: r('.react-flow__controls'), minimap: r('.react-flow__minimap') };
    });

    expect(boxes.canvas, 'the canvas must be mounted').not.toBeNull();
    expect(boxes.controls, 'the zoom controls must be mounted (they are the affordance, keep them)').not.toBeNull();
    expect(boxes.canvas.bottom, `canvas ends at ${boxes.canvas.bottom}, drawer starts at ${top}`).toBeLessThanOrEqual(top);
    expect(boxes.controls.bottom, `zoom controls end at ${boxes.controls.bottom}, drawer starts at ${top}`).toBeLessThanOrEqual(top);
    if (boxes.minimap) {
      expect(boxes.minimap.bottom, `minimap ends at ${boxes.minimap.bottom}, drawer starts at ${top}`).toBeLessThanOrEqual(top);
    }
  });
});
