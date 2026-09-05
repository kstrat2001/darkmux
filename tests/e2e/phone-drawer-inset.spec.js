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
const { test, expect } = require('@playwright/test');

const BASE = process.env.SHOT_BASE || 'http://127.0.0.1:47955/index.html';
const MISSION = 'demo-review-nameof-recency';
const DISPATCH = 'darkmux-compactor-compact-trajectories-2619114180';

const ROUTES = [
  ['fleet', '#lens=fleet'],
  ['console', '#lens=console'],
  ['runs', '#lens=runs'],
  ['machine', '#lens=machine'],
  ['mission', `#mission=${MISSION}`],
  ['dispatch', `#dispatch=${DISPATCH}`],
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

test.describe('every lens ends above the collapsed phone drawer', () => {
  for (const [name, hash] of ROUTES) {
    test(`${name}: no pinned control crosses the drawer bar`, async ({ page }) => {
      await page.goto(BASE + hash);
      await page.waitForSelector('.app-shell');
      await page.waitForTimeout(2000);
      const top = await drawerTop(page);
      expect(top, 'the phone drawer must be mounted at this viewport').not.toBeNull();
      // INTERSECTING the bar at rest, not merely "below y=drawerTop": most
      // pinned boxes here (`.sbar` activity segments, `.mm-row-*` meter
      // fills) are absolutely positioned inside a NORMAL-FLOW container far
      // down the document, so they sit at y=1200+ on a 844px viewport and
      // scroll into view above the bar like any other content. The defect is
      // a control that is ON SCREEN and cut in half by the bar.
      const vh = await page.evaluate(() => window.innerHeight);
      const offenders = (await pinnedControls(page)).filter((c) => c.top < vh && c.bottom > top);
      expect(offenders, `pinned controls under the drawer bar (top=${top}): ${JSON.stringify(offenders)}`).toEqual([]);
    });
  }

  test('mission GRAPH view: the canvas and its React Flow overlays stop at the drawer', async ({ page }) => {
    await page.goto(`${BASE}#mission=${MISSION}`);
    await page.waitForSelector('.evbtn');
    await page.waitForTimeout(1500);
    // The phone renders the TIMELINE first; the canvas is one tap away, and
    // it is the canvas whose overlays are bottom-pinned.
    await page.locator('.evbtn').click();
    await page.waitForSelector('.react-flow__controls', { timeout: 15000 });
    await page.waitForTimeout(2000);

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
