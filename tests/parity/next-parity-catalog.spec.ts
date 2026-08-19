// @ts-nocheck
// Packet 4 acceptance: `/next`'s catalog panel + replay-by-query surfaces
// vs the legacy goldens recorded this packet
// (`goldens/catalog-open.txt`/`mission-replay.txt`/`playback-date.txt` —
// `session-task-list.txt` already existed). Run via `bun run next-parity`
// (this file is registered in `next-playwright.config.js`'s `testMatch`,
// ADDITIVELY alongside `next-parity-runs.spec.ts`).
//
// Byte-parity scope, and why it's narrower than a full four-region compare
// for two of the three new goldens (documented here so nobody re-derives
// this from scratch, or worse, "fixes" the spec to force a false match):
//
//   - `catalog-open`: FULL byte parity, `#catpanel`'s region only (via the
//     new `extractCatalogOnlyText`/`catalogSectionOf` pair in
//     `lib/extract-next-lens.js`) — `CatalogPanel.tsx` is a genuine,
//     faithful port of `toggleCatalog()`'s row-building logic, so this is
//     the real spec-grade comparison this packet's brief asks for.
//   - `session-task-list`: ALSO full byte parity now (drill-in packet) —
//     `#stage` only (via `extractStageOnlyText`/`stageSectionOf`, the same
//     scoping `next-parity-runs.spec.ts` uses), against
//     `SessionReplay.tsx`'s real `runRegions()` render. This graduated from
//     the loose "48 records loaded" placeholder check the pre-drill-in
//     version of this test made — see the session test below and
//     `lenses/session/sessionRun.ts`'s own module doc for how this port
//     validated the derivation against this SAME golden at the pure-logic
//     layer first.
//   - `playback-date`: full byte parity as of #1800 P2, with TWO named
//     divergences from a pure legacy capture (the unported notes-modal
//     link, normalized in the test itself; the playback TRANSPORT rows,
//     rebaselined directly into the golden at #1869 — see below). It was
//     NOT parity before #1800 P2, for a reason worth keeping: this golden
//     captures legacy's fleet-hero render, and at the time `/next` had no
//     fleet-hero pipeline at all, so forcing parity would have meant
//     building one inside a catalog packet — scope creep wearing that
//     packet's clothes. Packet 5 built the real one; `PlaybackLens` now
//     COMPOSES it rather than growing a second, which is what made this a
//     small change instead of a large one. The narrower honest assertion
//     was the right call while it stood, and the golden earned its keep as
//     the record of what legacy does until the port could meet it.
//
//     #1869 added the playback transport (rewind/play/scrub/speed/clock),
//     and it is rendered INSIDE `#stage` in this port — a real, deliberate
//     layout difference from legacy, whose equivalent `.scrub` markup was a
//     body-level SIBLING of `.wrap`, outside `#stage` entirely
//     (viewer.html:854). That is why legacy's OWN capture of this golden
//     could not, structurally, contain any transport text — extracting
//     `#stage.innerText` from legacy never reached `.scrub` no matter what
//     it rendered. So `goldens/playback-date.txt` is no longer a pure
//     legacy capture: the five transport lines this port added
//     (⏮ / ▶ / 1× / the clock) are this port's own text, hand-added to the
//     golden, not bytes legacy ever emitted into this region. The
//     overlapping content diverges too, and on purpose: legacy's clock read
//     an ELAPSED duration (`fmt(t-tMin)+" / "+fmt(tMax-tMin)+" · N/M rec"`,
//     viewer.html:2619); this port renders absolute wall time
//     (`clkhm(t)` — "18:28 · 2008/2008 rec") instead, kept deliberately
//     because it matches how every other clock in this viewer already
//     reads and is more useful than an elapsed counter. Nothing else in
//     this golden changed at #1869.
//   - `mission-replay`: still NOT byte parity, and now for a DIFFERENT
//     reason than before #1868 — `mission-replay.txt` is legacy's fleet-hero
//     render for `#mission=<id>`, a route this port no longer stands in
//     front of at all: `MissionGraphLens` (#1868) renders the mission graph
//     itself in-place, graded byte-for-byte against its OWN goldens
//     (`goldens/mission-graph-*.txt`) by the SEPARATE `next-parity-graph`
//     suite. This golden survives only as the historical record of what
//     legacy's own daemon-less fallback once showed; the tests below assert
//     THIS port's real, current honest-error behavior (not-found, naming a
//     peer machine when the flow stream knows one) rather than comparing
//     against it.

import { test, expect } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { GOLDENS_DIR } from "./lib/paths.js";
// The FULL four-region extractor, shared verbatim with the legacy extraction
// (`extract.spec.ts`) exactly as `next-parity.spec.ts` uses it for the
// machine/fleet full-file compares. Imported alongside the stage-only helpers
// because this file now holds BOTH shapes: `session` compares `#stage` (its
// chrome is the ordinary live chrome, already covered by `fleet.txt`), while
// `playback` compares ALL FOUR regions — its topbar, crumb and meta are
// mode-specific and were exactly where the port diverged.
import { extractLensText } from "./lib/extract-lens.js";
import {
  extractCatalogOnlyText,
  catalogSectionOf,
  extractStageOnlyText,
  stageSectionOf,
  waitSettled,
  installFrozenClock,
  installCorpusRoutes,
  installBlankRoutes,
  loadMeta,
} from "./lib/extract-next-lens.js";

// Gitignored, repo-relative by default (`tests/parity/.gallery/4-catalog/`)
// — NOT an operator machine path (the corrected convention from the
// overnight runbook's own FOLLOW-UPS: packets 2 and 3 both originally
// hardcoded the operator's absolute scratchpad path here and had to be
// fixed post-QA). Override with `DARKMUX_GALLERY_DIR` for a real run; the
// `mkdirSync` lives in `beforeAll` (fires only when the suite actually
// RUNS), never at module scope.
const GALLERY_DIR = process.env.DARKMUX_GALLERY_DIR || path.join(__dirname, ".gallery", "4-catalog");
function shot(name: string) {
  return path.join(GALLERY_DIR, name);
}

function goldenText(label: string): string {
  return readFileSync(`${GOLDENS_DIR}/${label}.txt`, "utf8");
}

const CATALOG_SETTLED = "#catpanel .catrow";

test.describe("next-parity: catalog panel + replay-by-query (Packet 4)", () => {
  test.beforeAll(() => {
    mkdirSync(GALLERY_DIR, { recursive: true });
  });

  test("catalog panel matches catalog-open.txt's catalog region byte-for-byte", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html");
    await page.click(".catalog-toggle");
    // `.catrow` never appears in the synchronous "loading…" placeholder
    // (only `.cathdr` does) — same reasoning as extract.spec.ts's own
    // comment for the legacy harness.
    await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });

    const got = await extractCatalogOnlyText(page);
    expect(got, "catalog panel must match legacy's #catpanel byte-for-byte").toBe(catalogSectionOf(goldenText("catalog-open")));
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);

    await page.screenshot({ path: shot("catalog-open.png"), fullPage: true });
  });

  // QA must-fix (post-first-review): legacy has THREE ways to close
  // `#catpanel`, this component originally had zero — dropped both
  // dismissal handlers AND had a geometry bug (`.catpanel` at a page-level
  // `top:48px` overlapped the toggle button itself once `#meta` landed
  // above it) that made even the toggle structurally unclickable while
  // open. `CatalogPanel.test.tsx` (vitest/jsdom) covers the handler LOGIC;
  // these three tests are the real-browser regression proof — in
  // particular the geometry fix, which jsdom can't catch at all (jsdom has
  // no layout engine, so `page.click()`'s real actionability check —
  // "is this element visible AND not covered by another element at its
  // center point" — is exactly the mechanism QA used to catch the original
  // bug, and exactly what a mocked/jsdom click can't reproduce).
  test.describe("dismissal (real browser)", () => {
    test("Escape closes the panel", async ({ page }) => {
      const meta = loadMeta();
      await installFrozenClock(page, meta.frozen_clock_ms);
      installCorpusRoutes(page, meta);

      await page.goto("/index.html");
      await page.click(".catalog-toggle");
      await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });

      await page.keyboard.press("Escape");
      await expect(page.locator("#catpanel")).toHaveCount(0);
    });

    test("a click outside the panel closes it", async ({ page }) => {
      const meta = loadMeta();
      await installFrozenClock(page, meta.frozen_clock_ms);
      installCorpusRoutes(page, meta);

      await page.goto("/index.html");
      await page.click(".catalog-toggle");
      await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });

      // #stage is real page content outside both the toggle and the panel.
      await page.click("#stage", { position: { x: 5, y: 5 } });
      await expect(page.locator("#catpanel")).toHaveCount(0);
    });

    test("the toggle stays clickable while the panel is open — the geometry regression test", async ({ page }) => {
      const meta = loadMeta();
      await installFrozenClock(page, meta.frozen_clock_ms);
      installCorpusRoutes(page, meta);

      await page.goto("/index.html");
      await page.click(".catalog-toggle");
      await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });

      // This is the exact reproduction of QA's original finding: Playwright's
      // `click()` performs a real actionability check (element is visible,
      // stable, and NOT obscured by another element at its center point)
      // before clicking — it would TIME OUT here if the panel still
      // overlapped the toggle, the same way QA's `elementFromPoint` probe
      // caught it. A short timeout is deliberate: this must succeed FAST if
      // the geometry is right, not eventually.
      await page.click(".catalog-toggle", { timeout: 3000 });
      await expect(page.locator("#catpanel")).toHaveCount(0);
    });
  });

  test("#mission=<known-corpus-mission>: real fetch, honest not-found render naming the peer machine (#1868)", async ({ page }) => {
    // GRADUATED (#1868): this used to assert `MissionReplay`'s honest-empty
    // `/flow-mission/<id>` render before doing a full cross-document
    // navigation to `/mission/<id>/graph`. `MissionGraphLens` now renders
    // in-place instead, fetching `/mission/:id/graph.json` directly — this
    // corpus mission id has no recorded GRAPH fixture (only
    // `GRAPH_FIXTURE_MISSION_ID`, in `lib/graph-fixture.js`, does — see
    // `mock-routes.js`'s own comment on that explicit match), so the fetch
    // 404s and the lens takes its `errorNotFound` branch. This id DOES
    // appear in `flow-today.json` with a `machine_id` stamped, so the
    // lens's own peer-lookup (`lookupOwningMachine`) resolves it and names
    // the machine — the SAME honest behavior `mission-graph.html` itself
    // has always had (see `MissionGraphLens.tsx`'s own doc).
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html#mission=acp-ephemeral-pr-ship-1786152707367180000-5");
    await expect(page.getByText(/this mission ran on `MacBook-Pro`/i)).toBeVisible({ timeout: 15000 });
    expect(page.url()).toContain("/index.html");
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);

    await page.screenshot({ path: shot("mission-replay-empty.png"), fullPage: true });
  });

  test("bare #<date> hash matches playback-date.txt's #stage byte-for-byte (#1800 P2: a real historical render)", async ({
    page,
  }) => {
    // GRADUATED from the placeholder check this test used to make ("playback
    // for <date>" — honest at the time, since the lens rendered a named
    // not-ported notice). `PlaybackLens` now composes `FleetLens` over the
    // day's records with legacy's replay-mode branches taken, so this is held
    // to the same standard as every other lens here: the REAL browser's
    // `#stage.innerText` against the golden recorded from legacy.
    //
    // ONE runtime normalization applied below, per this file's "never a
    // silent fuzzy match" rule. Legacy's hybrid note ends with `history →`,
    // an anchor that opens a notes modal. That modal is NOT ported, and
    // `FleetLens` deliberately renders "(older notes exist)" instead —
    // rendering a link that looks clickable and does nothing would be a
    // trap control (see `FleetLens.tsx`'s own note). The INFORMATION is
    // identical ("there are older notes"); only the affordance differs.
    // Normalizing the port's text to legacy's keeps that one deliberate UX
    // divergence from masking any OTHER difference in the same string —
    // which is the whole reason to normalize narrowly and name it rather
    // than relaxing the compare.
    //
    // Delete this normalization when the notes modal lands.
    //
    // A SECOND divergence lives baked directly into the golden itself
    // (not a runtime normalization): the playback transport's five lines
    // (⏮ / ▶ / 1× / the clock), added at #1869. This port renders the
    // transport INSIDE `#stage`; legacy's own `.scrub` markup was a
    // body-level sibling of `.wrap`, OUTSIDE `#stage` — so legacy's own
    // capture of this golden could not, structurally, have contained that
    // text no matter what the transport rendered. See this file's own
    // module doc (top of file, the `playback-date` bullet) for the full
    // account, including the deliberate absolute-vs-elapsed clock format
    // difference. `goldens/playback-date.txt` is therefore no longer a
    // pure legacy capture for this one region — it is this port's own
    // transport text, hand-added and reviewed, not bytes legacy ever
    // emitted here.
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto(`/index.html#${meta.captured_prev_date}`);
    await waitSettled(page, expect, '.fleet-lens[data-state="loaded"]');

    const got = (await extractLensText(page)).replace(" (older notes exist)", " history →");
    expect(got, "playback must match legacy byte-for-byte, ALL regions").toBe(goldenText("playback-date"));
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);

    await page.screenshot({ path: shot("playback-date.png"), fullPage: true });
  });

  test("#session=task-list matches session-task-list.txt's #stage byte-for-byte (drill-in packet: real render, not a placeholder)", async ({
    page,
  }) => {
    // The one replay-by-query golden that already existed before this
    // packet (`goldens/session-task-list.txt`, Packet 0a) — now a REAL
    // byte-parity target: `SessionReplay.tsx` runs the corpus's 48 records
    // (`flow-session-task-list.json`) through `flowToRenderModel` +
    // `runRegions()` (`lenses/session/sessionRun.ts`) the same way legacy's
    // `renderSubsystem()` does, and this asserts the REAL BROWSER's
    // `#stage.innerText` matches the golden exactly — the same standard
    // every other lens in this suite is held to.
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html#session=task-list");
    await waitSettled(page, expect, '.session-run[data-state="data"]');

    const got = await extractStageOnlyText(page);
    const golden = readFileSync(`${GOLDENS_DIR}/session-task-list.txt`, "utf8");
    expect(got, "session drill-in must match legacy's #stage byte-for-byte").toBe(stageSectionOf(golden));
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);

    await page.screenshot({ path: shot("session-replay-notice.png"), fullPage: true });
  });

  test("render-sanity: zero pageerror, real region heights, no 390px overflow", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    const consoleErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });

    // Set AFTER navigation, not relying on the device preset — same gotcha
    // note as next-parity-runs.spec.ts's own render-sanity test.
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/index.html");
    await page.click(".catalog-toggle");
    await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });

    const panelBox = await page.locator("#catpanel").boundingBox();
    expect(panelBox, "#catpanel must have a real bounding box").not.toBeNull();
    expect(panelBox!.height).toBeGreaterThan(20);

    const stageBox = await page.locator("#stage").boundingBox();
    expect(stageBox, "#stage must have a real bounding box").not.toBeNull();
    expect(stageBox!.height).toBeGreaterThan(20);

    // `document.body.scrollWidth`, NOT `document.documentElement.scrollWidth`
    // — see next-parity-runs.spec.ts's identical comment for the
    // `overflow-x:hidden`-clamps-documentElement gotcha this avoids. The
    // catalog panel's own `max-width: min(420px, 92vw)` is the property
    // under test here — it's what keeps a 300px-min-width absolute overlay
    // from overflowing a 390px viewport.
    const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
    expect(overflow, "no horizontal document scroll at 390px, even with the catalog panel open").toBe(false);

    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
    expect(consoleErrors, `console.error events: ${consoleErrors.join("; ")}`).toHaveLength(0);

    const width = page.viewportSize()?.width;
    expect(width, "viewport must actually be 390px wide, not overridden by a device preset").toBe(390);

    await page.screenshot({ path: shot("catalog-390px.png"), fullPage: true });
  });
});

test.describe("next-parity: catalog panel red-prove (harness self-test, Packet 4)", () => {
  test("blank routes fail the catalog-open golden comparison", async ({ page }) => {
    await installFrozenClock(page, Date.UTC(2026, 0, 1));
    installBlankRoutes(page);

    await page.goto("/index.html");
    await page.click(".catalog-toggle");
    await expect(page.locator(CATALOG_SETTLED).first()).toBeAttached({ timeout: 15000 });
    const got = await extractCatalogOnlyText(page);
    expect(got, "blank-route extraction must NOT match the real catalog-open golden").not.toBe(
      catalogSectionOf(goldenText("catalog-open")),
    );
  });

  test("blank routes: mission-graph lens renders a visible not-found error, never a blank page (#1868)", async ({ page }) => {
    // GRADUATED (#1868): `installBlankRoutes` 404s `/mission/:id/graph.json`
    // for EVERY id (matching the real daemon's own 404 for "no local graph
    // for this mission" — see `mock-routes.js`'s #1868-packet-1 comment on
    // that route), the SAME response shape a genuinely unmatched id gets
    // under `installCorpusRoutes` too. Unlike the retired `MissionReplay`
    // (whose `/flow-mission/<id>` fetch DID distinguish "empty" 200 from a
    // real non-2xx error), `MissionGraphLens` has one honest branch for
    // "no local graph data" regardless of which of those two produced the
    // 404 — matching `mission-graph.html`'s own `errorNotFound` handling,
    // which never made that distinction either. `lookupOwningMachine` also
    // 404s here (every path is blanked), so the message falls back to the
    // generic "ephemeral or cleared run" wording, not a named peer.
    await installFrozenClock(page, Date.UTC(2026, 0, 1));
    installBlankRoutes(page);

    await page.goto("/index.html#mission=acp-ephemeral-pr-ship-1786152707367180000-5");
    await expect(page.getByText(/ephemeral or cleared run/i)).toBeVisible({ timeout: 15000 });
  });

  // (drill-in packet) The session drill-in's own red-prove, matching the
  // mission-replay one above — `installBlankRoutes` 404s `/flow-session/`
  // too, so this is a genuinely unreachable daemon, not the honest-empty
  // branch (which `installCorpusRoutes`' real fixture would exercise).
  test("blank routes: session replay renders a visible unreachable-daemon error, never a blank page", async ({ page }) => {
    await installFrozenClock(page, Date.UTC(2026, 0, 1));
    installBlankRoutes(page);

    await page.goto("/index.html#session=task-list");
    await expect(page.getByText(/couldn't reach \/flow-session\//i)).toBeVisible({ timeout: 15000 });
  });
});
