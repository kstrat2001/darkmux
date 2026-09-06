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
const fs = require('fs');
const path = require('path');

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

// (operator finding, real device — the OPEN drawer's own top inset) The
// tests above are about the COLLAPSED drawer's bar; this one is about the
// OPEN drawer's own body. On a real phone the Machine tab left ~43px of
// dead space between the tab row and the first legible text ("General") —
// `.hx-section`'s own separator spacing (16px margin + 1px border + 14px
// padding, meant to divide one titled block from the PREVIOUS one) stacked
// on top of `.phone-drawer__panel`'s own 12px `padding-top`, even though
// "General" is always the FIRST thing the panel renders and has no
// previous section to separate from. The Events tab's own head padding
// already sits at this sheet's intended 8-12px gutter — the fix must not
// widen that one while narrowing the other.
test.describe('phone drawer open: content starts right under the tab row, not ~40px under it', () => {
  const GUTTER_MAX_PX = 16;

  /** Opens the sheet to `tab` from `url`, waiting for `ready` (a hook proving
   * the underlying lens actually mounted — see this file's own doc on why
   * every route here waits for one before measuring) and for the sheet's
   * own open state, so a measurement never race against the open transition. */
  async function openDrawerTab(page, url, ready, tab) {
    await page.goto(url);
    await expect(page.locator(ready).first(), `${url} never rendered`).toBeVisible();
    await page.locator(`[data-act="phone-drawer-tab-${tab}"]`).click();
    await expect(page.locator('.phone-drawer')).toHaveClass(/phone-drawer--open/);
  }

  /** The tab row's own bottom edge, and the top of `sel` (this tab's own
   * "first real content row" hook) — both real painted boxes, not inferred
   * from any CSS value, so the assertion holds regardless of which rule
   * ends up producing the gutter. */
  async function gapAbove(page, sel) {
    return page.evaluate((sel) => {
      const bar = document.querySelector('.phone-drawer__bar');
      const first = document.querySelector(sel);
      if (!bar || !first) return { barBottom: null, top: null, gap: null };
      const barBottom = bar.getBoundingClientRect().bottom;
      const top = first.getBoundingClientRect().top;
      return { barBottom, top, gap: top - barBottom };
    }, sel);
  }

  test('Machine info tab: the General section starts within the gutter', async ({ page }) => {
    await openDrawerTab(page, '/index.html#lens=fleet', '.savrow', 'machine');
    // The panel's own first child — whichever section
    // `machineStatsContent.tsx` renders first (always "General" today, per
    // that file's own doc), independent of which row inside it happens to
    // carry the first legible label.
    const { gap } = await gapAbove(page, '.phone-drawer__panel > *:first-child');
    expect(gap, 'the Machine tab panel never mounted a first section').not.toBeNull();
    expect(gap, `first section starts ${gap}px below the tab row (want <= ${GUTTER_MAX_PX}px)`).toBeLessThanOrEqual(
      GUTTER_MAX_PX,
    );
  });

  test('Events tab: the search row starts within the gutter', async ({ page }) => {
    await openDrawerTab(page, '/index.html#lens=fleet', '.savrow', 'events');
    const { gap } = await gapAbove(page, '.phone-drawer__body .eventlog__searchbox input');
    expect(gap, 'the Events tab never mounted its search row').not.toBeNull();
    expect(gap, `search row starts ${gap}px below the tab row (want <= ${GUTTER_MAX_PX}px)`).toBeLessThanOrEqual(
      GUTTER_MAX_PX,
    );
  });
});

// (operator finding, real iPhone, 2026-09-06 evening, daemon build bf128cb9 /
// 583c384e) The Events tab's own list scrolled HORIZONTALLY — rows rendered
// at ~78% of the panel width with an empty band on the right, and the list
// sat scrolled so a row's left edge (the time column's first digit) was
// clipped. Instrumented (`page.evaluate` walking every element under
// `[data-act="phone-drawer-body"]` for `getBoundingClientRect().right >
// panel.right + 1` or `scrollWidth > clientWidth`) against a 700-record
// fixture carrying one genuinely unbreakable token (a 72-char hex string
// with no space or hyphen for the browser's default line breaker to land
// on, standing in for a real `dispatch.tool` arg or session id shaped the
// same way): `.eventlog__rec` measured `scrollWidth: 537` against a
// `clientWidth: 364` — a `.preview-text` span rendered that token as ONE
// unbreakable inline run past the row's own right edge, and because
// `.eventlog__body` (the row's scroll ancestor, `#logbody`) is `overflow:
// auto` on BOTH axes, that overflow became real horizontal scroll room for
// the whole list — every ordinary (short) row then rendered inside the
// widened scrollable content at less than the panel's own width, which is
// the "78%, empty band on the right" the operator saw. The fix
// (`ui/src/styles.css`, `.eventlog__rec`) adds `overflow-wrap: anywhere` —
// normal text still wraps at its existing spaces/hyphens exactly as before;
// only a run with NO such break point gets broken now, inside the row
// instead of past it. Note: the Events tab (unlike the Machine tab) never
// mounts a `.phone-drawer__panel` wrapper — `PhoneDrawer.tsx` renders
// `<EventLogColumn>` directly into `.phone-drawer__body` — so `panel` below
// is that body element, the real outer bound for this tab's content, and
// `#logbody` (`.eventlog__body`) is the actual scroll container that was
// growing.
test.describe('phone drawer Events tab: the event list never scrolls horizontally', () => {
  const FIXTURE = fs.readFileSync(
    path.join(__dirname, '..', 'fixtures', 'hscroll-overflow-flow.jsonl'),
    'utf8',
  );

  async function openEventsWithOverflowFixture(page) {
    await page.route('**/filters-overflow-flow.jsonl', (route) =>
      route.fulfill({ status: 200, contentType: 'application/x-ndjson', body: FIXTURE }),
    );
    await page.goto('/index-filters-overflow.html');
    await page.click('[data-act="phone-drawer-tab-events"]');
    await expect(page.locator('.eventlog__rec').first()).toBeVisible();
  }

  async function measure(page) {
    return page.evaluate(() => {
      const panel = document.querySelector('[data-act="phone-drawer-body"]');
      const list = document.getElementById('logbody');
      const firstRow = document.querySelector('.eventlog__rec');
      const follow = document.getElementById('follow');
      const fbtn = document.getElementById('fbtn');
      const panelRect = panel.getBoundingClientRect();
      const rowRect = firstRow.getBoundingClientRect();
      const followRect = follow.getBoundingClientRect();
      const fbtnRect = fbtn.getBoundingClientRect();
      return {
        panelScrollWidth: panel.scrollWidth,
        panelClientWidth: panel.clientWidth,
        listScrollWidth: list.scrollWidth,
        listClientWidth: list.clientWidth,
        panelLeft: panelRect.left,
        panelRight: panelRect.right,
        rowLeft: rowRect.left,
        rowRight: rowRect.right,
        followWidth: followRect.width,
        followHeight: followRect.height,
        fbtnWidth: fbtnRect.width,
        fbtnHeight: fbtnRect.height,
      };
    });
  }

  for (const viewport of [
    { width: 390, height: 844 },
    { width: 320, height: 568 },
  ]) {
    test(`no horizontal overflow at ${viewport.width}px`, async ({ page }) => {
      await page.setViewportSize(viewport);
      await openEventsWithOverflowFixture(page);
      const m = await measure(page);

      expect(m.panelScrollWidth, `drawer body scrollWidth ${m.panelScrollWidth} vs clientWidth ${m.panelClientWidth}`).toBeLessThanOrEqual(m.panelClientWidth + 1);
      expect(m.listScrollWidth, `event list scrollWidth ${m.listScrollWidth} vs clientWidth ${m.listClientWidth}`).toBeLessThanOrEqual(m.listClientWidth + 1);
      expect(m.rowLeft, `first row's left edge (${m.rowLeft}) is left of the panel's own left (${m.panelLeft})`).toBeGreaterThanOrEqual(m.panelLeft - 1);
      expect(m.panelRight - m.rowRight, `first row's right edge is ${m.panelRight - m.rowRight}px from the panel's inner right edge (want <= 20px, i.e. the row fills the width)`).toBeLessThanOrEqual(20);

      // (operator finding, round 2, 2026-09-06) 29.7x36 (#fbtn) and 33x36
      // (#follow) measured under the 44px Apple-minimum tap target — a
      // later, more specific `.phone-drawer__body` selector was overriding
      // the codebase's own existing `@media (max-width: 768px)` 44x44
      // rule. Both icon buttons must be >= 44x44 in the drawer regardless
      // of viewport.
      expect(m.followWidth, `#follow is ${m.followWidth}x${m.followHeight}, want >= 44x44`).toBeGreaterThanOrEqual(44);
      expect(m.followHeight, `#follow is ${m.followWidth}x${m.followHeight}, want >= 44x44`).toBeGreaterThanOrEqual(44);
      expect(m.fbtnWidth, `#fbtn is ${m.fbtnWidth}x${m.fbtnHeight}, want >= 44x44`).toBeGreaterThanOrEqual(44);
      expect(m.fbtnHeight, `#fbtn is ${m.fbtnWidth}x${m.fbtnHeight}, want >= 44x44`).toBeGreaterThanOrEqual(44);
    });
  }
});
