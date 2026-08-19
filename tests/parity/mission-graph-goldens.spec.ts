// @ts-nocheck
// #1868 packet 1 acceptance: capture PARITY GOLDENS from the CURRENT
// standalone `crates/darkmux-serve/assets/mission-graph.html` page (served
// live at `/mission/:id/graph`) BEFORE it gets folded into the React port
// (the later packet in #1868). This is the frozen spec a future
// `next-parity-graph.spec.ts` grades the ported lens against, the same
// shape as every `next-parity*.spec.ts` sibling in this directory, just for
// a page that isn't a build artifact (no `next.html` bundle to stage; see
// this suite's own `mission-graph-goldens.playwright.config.js` for why
// baseURL points straight at the live daemon instead of a staged static
// server).
//
// The fixture: `GRAPH_FIXTURE_MISSION_ID` (`lib/graph-fixture.js`), a
// synthetic sanity-check mission with 3 phases, 5 tasks, 5 steps, 9 edges,
// 8 flow-mission records, verified live against the daemon at authoring
// time, containing no client-identifying content. Its two endpoints
// (`/mission/:id/graph.json`, `/flow-mission/:id`) are recorded into
// `corpus/mission-graph-sanity.json` / `corpus/flow-mission-sanity.json` by
// `record.mjs`, replayed here (with every OTHER endpoint the page also
// fetches, `/flow/<today>` and `/flow/<today>/stream`) through
// `lib/mock-routes.js`'s `installCorpusRoutes`, exactly like every other
// suite in this directory.
//
// Two renderers, one page (`timelineActive()` in mission-graph.html):
// `viewMode` starts at `"auto"` and is NEVER persisted anywhere (unlike the
// minimap's `dmux.mmap` localStorage key, which is a DIFFERENT, unrelated
// setting). The renderer choice under `"auto"` is decided purely by
// `isNarrowViewport()` (`window.innerWidth <= 700`), read once at mount and
// kept live by a `resize` listener. So this suite reaches BOTH renderers
// the same way an operator's browser width would: a desktop viewport
// (1280x900) renders the React Flow canvas; a mobile viewport (390x844)
// renders the vertical timeline. No localStorage write and no click on the
// page's own "switch renderer" toggle button is needed for either golden.
//
// The events panel (`.evpanel` / `.evrow` rows), by contrast, defaults
// OPEN on desktop and CLOSED on mobile (`useState(!isNarrowViewport())`),
// so the canvas test needs no click to see event rows, but the timeline
// test does (see that test's own comment).
//
// SSE stream: mocked as a NEVER-RESOLVING pending request (the technique
// `next-parity-live.spec.ts`'s own module doc names as point 2, since a
// `route.fulfill`-delivered stream response is a COMPLETE HTTP response,
// not a true open-ended one, so it closes immediately and forces a
// reconnect the corpus's own inert-empty-stream mock in `mock-routes.js`
// would otherwise trigger on a ~3s cadence). A pending request never
// reaches `onopen`/`onerror`, so the connection state never flickers during
// extraction: deterministic goldens, no race with a reconnect timer. (In
// this fixture's specific case the `live`/`reconnecting` pill is hidden
// entirely anyway, since the mission is `finalized`; see
// mission-graph.html's own comment on `statusRank(...) >= 2`. The
// technique is applied regardless, both for the header's OTHER content and
// so this suite doesn't silently depend on that fixture-specific fact.)
const { test, expect } = require("@playwright/test");
const { mkdirSync, writeFileSync, readFileSync } = require("node:fs");
const path = require("node:path");
const { installFrozenClock } = require("./lib/extract-lens.js");
const { loadMeta, installCorpusRoutes, installBlankRoutes } = require("./lib/mock-routes.js");
const { GRAPH_FIXTURE_MISSION_ID } = require("./lib/graph-fixture.js");
const { GOLDENS_DIR } = require("./lib/paths.js");
const {
  extractEdgeCount,
  extractCanvasGolden,
  extractTimelineGolden,
  expandAllTimelineTasks,
} = require("./lib/extract-graph.js");

const CANVAS_GOLDEN_PATH = path.join(GOLDENS_DIR, "mission-graph-canvas.txt");
const TIMELINE_GOLDEN_PATH = path.join(GOLDENS_DIR, "mission-graph-timeline.txt");

const DESKTOP_VIEWPORT = { width: 1280, height: 900 };
const MOBILE_VIEWPORT = { width: 390, height: 844 };

/** See the module doc's "SSE stream" paragraph. Registered AFTER
 * `installCorpusRoutes` so it wins for the stream path (Playwright checks
 * the LATEST-registered `page.route` handler first) and defers
 * (`route.fallback()`) to the corpus handler for everything else. */
async function installHangingSseStream(page, meta) {
  const streamPath = `/flow/${meta.captured_date}/stream`;
  await page.route("**/*", async (route) => {
    const url = new URL(route.request().url());
    if (url.pathname === streamPath) {
      await new Promise(() => {}); // deliberately never resolves
      return;
    }
    return route.fallback();
  });
}

test.describe("mission-graph goldens (#1868 packet 1)", () => {
  test("canvas: desktop viewport (viewMode=auto -> canvas)", async ({ page }) => {
    const meta = loadMeta();
    await page.setViewportSize(DESKTOP_VIEWPORT);
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    await installHangingSseStream(page, meta);

    await page.goto(`/mission/${GRAPH_FIXTURE_MISSION_ID}/graph`);

    // Settle: the canvas renderer's own node vocabulary attaches once
    // graph.json resolves; React Flow's edges paint one tick after nodes in
    // its own internal layout pass, so they get their own stabilizing wait.
    await expect(page.locator(".phasegroup").first()).toBeAttached({ timeout: 15000 });
    await expect(page.locator(".mnode").first()).toBeAttached({ timeout: 15000 });
    await expect(async () => {
      expect(await page.locator(".react-flow__edge").count()).toBeGreaterThan(0);
    }).toPass({ timeout: 15000, intervals: [200] });

    // The events panel is OPEN by default on a desktop (non-narrow)
    // viewport (`useState(!isNarrowViewport())`); no click needed here,
    // unlike the timeline test below.
    await expect(async () => {
      expect(await page.locator(".evrow").count()).toBeGreaterThan(0);
    }).toPass({ timeout: 15000, intervals: [200] });

    const phaseCount = await page.locator(".phasegroup").count();
    const nodeCount = await page.locator(".mnode").count();
    const edgeCount = await extractEdgeCount(page);
    const eventCount = await page.locator(".evrow").count();
    // The fixture is a fixed, known shape, so pin the exact numbers (not
    // just ">0") so a future regression that silently drops a
    // node/edge/event fails loudly here even before the golden-text diff
    // below would also catch it.
    expect(phaseCount, "phasegroups (3 phase nodes)").toBe(3);
    expect(nodeCount, "mission nodes (5 task nodes)").toBe(5);
    // 9 recorded edges (5 `contains` + 4 `depends_on`) minus the 5
    // `contains` edges (drawn by phase-container ENCLOSURE, not a line;
    // see `toRfEdges`'s own comment in mission-graph.html) plus 2
    // client-synthesized `phase_order` edges (3 phases -> 2 consecutive
    // pairs) = 6.
    expect(edgeCount, "react-flow edges").toBe(6);
    expect(eventCount, "event rows (8 flow-mission records)").toBe(8);

    const golden = await extractCanvasGolden(page);
    expect(golden.length, "captured golden must not be empty").toBeGreaterThan(0);
    mkdirSync(GOLDENS_DIR, { recursive: true });
    writeFileSync(CANVAS_GOLDEN_PATH, golden, "utf8");
  });

  test("timeline: mobile viewport (viewMode=auto -> timeline)", async ({ page }) => {
    const meta = loadMeta();
    await page.setViewportSize(MOBILE_VIEWPORT);
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    await installHangingSseStream(page, meta);

    await page.goto(`/mission/${GRAPH_FIXTURE_MISSION_ID}/graph`);

    await expect(page.locator(".tlphase").first()).toBeAttached({ timeout: 15000 });
    await expect(page.locator(".tltask").first()).toBeAttached({ timeout: 15000 });

    // Every task card starts COLLAPSED (`expanded` state initializes to
    // `{}`; nothing auto-opens, see `lib/extract-graph.js`'s own doc on
    // `expandAllTimelineTasks`). Expand every one so `.tlt-step` rows exist
    // in the DOM to capture. The deliverable calls for step-level content
    // in this golden too, and a collapsed task renders none at all.
    await expandAllTimelineTasks(page, expect);
    await expect(async () => {
      expect(await page.locator(".tlt-step").count()).toBeGreaterThan(0);
    }).toPass({ timeout: 15000, intervals: [200] });

    // The events panel is CLOSED by default on a narrow (mobile) viewport
    // (`useState(!isNarrowViewport())`, the same expression as the canvas
    // test, opposite result at this width); click it open.
    await page.getByTitle("mission events").click();
    await expect(async () => {
      expect(await page.locator(".evrow").count()).toBeGreaterThan(0);
    }).toPass({ timeout: 15000, intervals: [200] });

    const phaseCount = await page.locator(".tlphase").count();
    const taskCount = await page.locator(".tltask").count();
    const stepCount = await page.locator(".tlt-step").count();
    const eventCount = await page.locator(".evrow").count();
    expect(phaseCount, "timeline phases").toBe(3);
    expect(taskCount, "timeline tasks").toBe(5);
    expect(stepCount, "timeline steps (all 5 tasks expanded, 1 step each)").toBe(5);
    expect(eventCount, "event rows (8 flow-mission records)").toBe(8);

    const golden = await extractTimelineGolden(page);
    expect(golden.length, "captured golden must not be empty").toBeGreaterThan(0);
    mkdirSync(GOLDENS_DIR, { recursive: true });
    writeFileSync(TIMELINE_GOLDEN_PATH, golden, "utf8");
  });
});

// Mirrors the retired `redprove.spec.ts`'s doctrine (operator: "a probe that
// passes without executing is worse than no probe"): a blank/unreachable
// daemon must not produce the same extracted text as the real golden. Runs
// AFTER the capture tests above (this file's fixed definition order, one
// worker, `fullyParallel: false`, see the config) so the golden files on
// disk are the ones this run just wrote, not stale leftovers.
test.describe("mission-graph goldens red-prove (harness self-test)", () => {
  test("blank routes must not match the canvas golden", async ({ page }) => {
    const meta = loadMeta();
    await page.setViewportSize(DESKTOP_VIEWPORT);
    await installFrozenClock(page, meta.frozen_clock_ms);
    installBlankRoutes(page);

    await page.goto(`/mission/${GRAPH_FIXTURE_MISSION_ID}/graph`);
    // graph.json 404s under a blank daemon -> the page's own calm
    // not-available message (`.msg`, inside the same `.top`-carrying shell
    // every state renders; see mission-graph.html's `minimalTopBar`).
    await expect(page.locator(".msg")).toBeAttached({ timeout: 15000 });
    expect(await page.locator(".mnode").count(), "no graph data -> no mission nodes").toBe(0);
    expect(await page.locator(".phasegroup").count(), "no graph data -> no phase groups").toBe(0);

    const golden = readFileSync(CANVAS_GOLDEN_PATH, "utf8");
    const blankText = await extractCanvasGolden(page);
    expect(blankText, "a blank/unreachable daemon must not produce the real golden's text").not.toBe(golden);
  });

  test("blank routes must not match the timeline golden", async ({ page }) => {
    const meta = loadMeta();
    await page.setViewportSize(MOBILE_VIEWPORT);
    await installFrozenClock(page, meta.frozen_clock_ms);
    installBlankRoutes(page);

    await page.goto(`/mission/${GRAPH_FIXTURE_MISSION_ID}/graph`);
    await expect(page.locator(".msg")).toBeAttached({ timeout: 15000 });
    expect(await page.locator(".tlphase").count(), "no graph data -> no timeline phases").toBe(0);
    expect(await page.locator(".tltask").count(), "no graph data -> no timeline tasks").toBe(0);

    const golden = readFileSync(TIMELINE_GOLDEN_PATH, "utf8");
    const blankText = await extractTimelineGolden(page);
    expect(blankText, "a blank/unreachable daemon must not produce the real golden's text").not.toBe(golden);
  });
});
