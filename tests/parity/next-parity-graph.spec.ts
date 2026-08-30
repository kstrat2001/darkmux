// @ts-nocheck
// #1868 packet 2's acceptance gate: grade `MissionGraphLens` (the React
// port's `#mission=<id>` lens, `ui/src/lenses/mission/`) against the SAME
// `goldens/mission-graph-canvas.txt` / `goldens/mission-graph-timeline.txt`
// packet 1 captured from the standalone `crates/darkmux-serve/assets/
// mission-graph.html` page — before ANY of this packet's code existed (see
// that packet's own `mission-graph-goldens.spec.ts` module doc).
//
// Same fixture, same corpus, same technique as packet 1's capture:
// `GRAPH_FIXTURE_MISSION_ID` (`lib/graph-fixture.js`), replayed through
// `lib/mock-routes.js`'s `installCorpusRoutes` (already wired for this
// fixture's `/mission/:id/graph.json` and mission-scoped `/flow-mission/:id`
// endpoints — see that file's own #1868-packet-1 comments), plus a hanging
// SSE stream (see `installHangingSseStream` below — identical technique to
// packet 1's own, needed for the SAME reason: a `route.fulfill`-delivered
// stream is a COMPLETE response, not open-ended, and this port's
// `useLiveTail` (mounted by `MissionGraphLens` itself, see that
// component's own doc) reconnects on that closed response otherwise).
//
// Grading, per section:
//   - phasegroups / nodes / edges / timeline: BYTE-FOR-BYTE against the
//     golden, using the EXACT SAME extractors packet 1's capture used
//     (`extractPhaseGroupsText`/`extractMissionNodesText`/`extractEdgeCount`/
//     `extractTimelineNodesText`) — zero new extraction code, because
//     `MissionCanvas.tsx`/`MissionTimelineView.tsx` keep `.phasegroup`/
//     `.mnode`/`.steprow`/`.tlphase`/`.tltask`/`.tlt-step` identical to the
//     standalone page on purpose.
//   - header / events: NORMALIZED comparisons via the new port-side
//     extractors in `lib/extract-graph.js` (`extractPortHeaderText`/
//     `extractPortEventsText`), since the port has its own chrome (a real
//     app-shell masthead) and its events pane is `EventLogColumn`, not the
//     standalone page's `.evrow` markup — see that module's own doc on each
//     extractor for exactly what's normalized and why. The events
//     comparison is still a REAL byte-equality check on the normalized
//     shape (`time | action | subject`), not a fuzzy match.
//
// Two body-renderer viewports, same as packet 1: 1280x900 (desktop -> React
// Flow canvas) and 390x844 (mobile -> the vertical timeline).
const { test, expect } = require("@playwright/test");
const { installFrozenClock } = require("./lib/extract-lens.js");
const { loadMeta, installCorpusRoutes, installBlankRoutes } = require("./lib/mock-routes.js");
const { GRAPH_FIXTURE_MISSION_ID } = require("./lib/graph-fixture.js");
const { GOLDENS_DIR } = require("./lib/paths.js");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const {
  extractPhaseGroupsText,
  extractMissionNodesText,
  extractEdgeCount,
  extractTimelineNodesText,
  expandAllTimelineTasks,
  extractPortHeaderText,
  headerFactsOf,
  extractPortEventsText,
  sectionOf,
  eventsBodyOf,
} = require("./lib/extract-graph.js");

const CANVAS_GOLDEN = readFileSync(path.join(GOLDENS_DIR, "mission-graph-canvas.txt"), "utf8");
const TIMELINE_GOLDEN = readFileSync(path.join(GOLDENS_DIR, "mission-graph-timeline.txt"), "utf8");

const DESKTOP_VIEWPORT = { width: 1280, height: 900 };
const MOBILE_VIEWPORT = { width: 390, height: 844 };

/** See `mission-graph-goldens.spec.ts`'s own "SSE stream" module-doc
 * paragraph for why this is needed — identical technique, this suite's own
 * copy since it's a different `page.route` handler chain (this suite's
 * stream path is under `/next.html`'s own served origin, not the standalone
 * page's). Registered AFTER `installCorpusRoutes` so it wins for the stream
 * path (Playwright checks the LATEST-registered handler first) and defers
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

test.describe("next-parity-graph: MissionGraphLens vs. the standalone page's frozen goldens (#1868 packet 2)", () => {
  test("canvas: desktop viewport matches phasegroups/nodes/edges byte-for-byte; header/events match normalized", async ({ page }) => {
    const meta = loadMeta();
    await page.setViewportSize(DESKTOP_VIEWPORT);
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    await installHangingSseStream(page, meta);

    await page.goto(`/index.html#mission=${GRAPH_FIXTURE_MISSION_ID}`);

    await expect(page.locator(".phasegroup").first()).toBeAttached({ timeout: 15000 });
    await expect(page.locator(".mnode").first()).toBeAttached({ timeout: 15000 });
    await expect(async () => {
      expect(await page.locator(".react-flow__edge").count()).toBeGreaterThan(0);
    }).toPass({ timeout: 15000, intervals: [200] });
    // (mainstay-unification packet) `MissionGraphLens` no longer mounts its
    // own `EventLogColumn` — it reports its scoped events upward via
    // `onEvents`, and `App.tsx` feeds the SAME shared/mainstay column every
    // other route already uses (a desktop route's standalone mount, always
    // visible now that mission is no longer excluded — see
    // `lib/route.ts`'s `showsEventLog` doc). So the events pane lives
    // OUTSIDE `.missionlens`'s own DOM subtree now; select it unscoped.
    await expect(async () => {
      expect(await page.locator(".eventlog__rec").count()).toBeGreaterThan(0);
    }).toPass({ timeout: 15000, intervals: [200] });

    // Pinned counts — same fixture, same expectations as packet 1's own
    // capture test, so a silent node/edge/event drop fails loudly here too.
    expect(await page.locator(".phasegroup").count(), "phasegroups (3 phase nodes)").toBe(3);
    expect(await page.locator(".mnode").count(), "mission nodes (5 task nodes)").toBe(5);
    expect(await extractEdgeCount(page), "react-flow edges").toBe(6);
    expect(await page.locator(".eventlog__rec").count(), "event rows (8 flow-mission records)").toBe(8);

    const [phasegroups, nodes, edgeCount] = await Promise.all([
      extractPhaseGroupsText(page),
      extractMissionNodesText(page),
      extractEdgeCount(page),
    ]);
    const gotPhasegroups = `=== phasegroups ===\n${phasegroups.length ? phasegroups.join("\n") : "(none)"}\n\n`;
    const gotNodes = `=== nodes ===\n${nodes.length ? nodes.join("\n\n") : "(none)"}\n\n`;
    const gotEdges = `=== edges ===\nedge_count: ${edgeCount}\n\n`;

    expect(gotPhasegroups, "phasegroups must match the standalone-page golden byte-for-byte").toBe(sectionOf(CANVAS_GOLDEN, "phasegroups"));
    expect(gotNodes, "mission nodes must match the standalone-page golden byte-for-byte").toBe(sectionOf(CANVAS_GOLDEN, "nodes"));
    expect(gotEdges, "edge count must match the standalone-page golden byte-for-byte").toBe(sectionOf(CANVAS_GOLDEN, "edges"));

    const gotHeader = await extractPortHeaderText(page);
    expect(gotHeader, "the port's own header must name the SAME mission id + status as the golden").toBe(headerFactsOf(CANVAS_GOLDEN));

    const gotEvents = (await extractPortEventsText(page)).join("\n");
    expect(gotEvents, "EventLogColumn's rows must carry the SAME time|action|subject triples as the standalone page's .evrow list").toBe(
      eventsBodyOf(CANVAS_GOLDEN),
    );
  });

  test("timeline: mobile viewport matches the interleaved phase/task/step vocabulary byte-for-byte; header/events match normalized", async ({
    page,
  }) => {
    const meta = loadMeta();
    await page.setViewportSize(MOBILE_VIEWPORT);
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    await installHangingSseStream(page, meta);

    await page.goto(`/index.html#mission=${GRAPH_FIXTURE_MISSION_ID}`);

    await expect(page.locator(".tlphase").first()).toBeAttached({ timeout: 15000 });
    await expect(page.locator(".tltask").first()).toBeAttached({ timeout: 15000 });

    // Every task starts collapsed — expand all so `.tlt-step` rows exist to
    // capture, matching packet 1's own capture test exactly.
    await expandAllTimelineTasks(page, expect);
    await expect(async () => {
      expect(await page.locator(".tlt-step").count()).toBeGreaterThan(0);
    }).toPass({ timeout: 15000, intervals: [200] });

    // (mainstay-unification packet) There is no more per-lens "mission
    // events" toggle — `MissionGraphLens` reports its events upward instead
    // of rendering its own pane (see the canvas test's own doc above). On a
    // narrow viewport the mainstay column lives inside the phone drawer's
    // Events tab, closed by default; open it the same way an operator would.
    await page.locator('[data-act="phone-drawer-tab-events"]').click();
    await expect(async () => {
      expect(await page.locator(".eventlog__rec").count()).toBeGreaterThan(0);
    }).toPass({ timeout: 15000, intervals: [200] });

    expect(await page.locator(".tlphase").count(), "timeline phases").toBe(3);
    expect(await page.locator(".tltask").count(), "timeline tasks").toBe(5);
    expect(await page.locator(".tlt-step").count(), "timeline steps (all 5 tasks expanded, 1 step each)").toBe(5);
    expect(await page.locator(".eventlog__rec").count(), "event rows (8 flow-mission records)").toBe(8);

    const nodes = await extractTimelineNodesText(page);
    const gotTimeline = `=== timeline ===\n${nodes.length ? nodes.join("\n") : "(none)"}\n\n`;
    expect(gotTimeline, "the interleaved phase/task/step vocabulary must match the standalone-page golden byte-for-byte").toBe(
      sectionOf(TIMELINE_GOLDEN, "timeline"),
    );

    const gotHeader = await extractPortHeaderText(page);
    expect(gotHeader, "the port's own header must name the SAME mission id + status as the golden").toBe(headerFactsOf(TIMELINE_GOLDEN));

    const gotEvents = (await extractPortEventsText(page)).join("\n");
    expect(gotEvents, "EventLogColumn's rows must carry the SAME time|action|subject triples as the standalone page's .evrow list").toBe(
      eventsBodyOf(TIMELINE_GOLDEN),
    );
  });
});

// Mirrors the doctrine every next-parity* sibling's own red-prove section
// cites (operator: "a probe that passes without executing is worse than no
// probe"): a blank/unreachable daemon must not produce the same extracted
// text as the real golden.
test.describe("next-parity-graph red-prove (harness self-test)", () => {
  test("a blank route (#mission=<unknown-id>) never matches the real golden's phasegroups/nodes/timeline", async ({ page }) => {
    const meta = loadMeta();
    await page.setViewportSize(DESKTOP_VIEWPORT);
    await installFrozenClock(page, meta.frozen_clock_ms);
    installBlankRoutes(page);

    // installBlankRoutes 404s /mission/:id/graph.json for EVERY id — this
    // fixture id included, matching packet 1's own red-prove (a blank
    // daemon's 404 maps to the SAME "no mission found" honest render the
    // real daemon gives for a genuinely absent mission — see
    // `MissionGraphLens.tsx`'s own doc on why this port collapses "actually
    // unreachable" and "not found" into one honest branch, unlike
    // `MissionReplay`'s old two-branch shape).
    await page.goto(`/index.html#mission=${GRAPH_FIXTURE_MISSION_ID}`);
    await expect(page.getByRole("alert")).toBeVisible({ timeout: 15000 });
    expect(await page.locator(".mnode").count(), "no graph data -> no mission nodes").toBe(0);
    expect(await page.locator(".phasegroup").count(), "no graph data -> no phase groups").toBe(0);

    const nodes = await extractMissionNodesText(page);
    const gotNodes = `=== nodes ===\n${nodes.length ? nodes.join("\n\n") : "(none)"}\n\n`;
    expect(gotNodes, "a blank/unreachable daemon must not produce the real golden's node text").not.toBe(sectionOf(CANVAS_GOLDEN, "nodes"));
  });
});
