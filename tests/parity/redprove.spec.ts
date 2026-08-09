// @ts-nocheck
// Red-prove — the harness's own self-test. Runs the IDENTICAL extraction
// (lib/extract-lens.js, imported verbatim, not re-implemented) against a
// blank page — every endpoint the corpus-backed harness serves instead
// answers 404/empty here — and asserts every resulting text DIFFERS from the
// real golden already on disk. Operator doctrine: "a probe that passes
// without executing is worse than no probe" — if extraction against a blank
// page ever matched a real golden, that golden would be worthless as a
// parity spec, so this file exists to make that failure mode loud instead of
// silent.
//
// The "settled" markers here deliberately DIFFER from extract.spec.ts's for
// one lens (machine): extract.spec.ts's `.memcard` selector requires REAL
// data, which the blank page can never produce (its resources fetch always
// fails, landing renderMachine() in the permanent "daemon not reachable"
// branch, never the loaded one). Waiting on `.memcard` here would hang until
// timeout instead of cleanly observing the settled-but-empty state. Each
// lens's blank-page marker is documented below with the branch it targets.
//
// Depends on extract.spec.ts having already run and written goldens/*.txt —
// run `bun run check` (or `rebaseline` then `redprove`), not this file alone
// against an empty goldens/ dir.

import { test, expect } from "@playwright/test";
import { readFileSync, existsSync } from "node:fs";
import { GOLDENS_DIR } from "./lib/paths.js";
import { installBlankRoutes } from "./lib/mock-routes.js";
import { extractLensText, extractLensTextWithCatalog, waitSettled, installFrozenClock, regionText } from "./lib/extract-lens.js";

function existingGolden(label) {
  const p = `${GOLDENS_DIR}/${label}.txt`;
  if (!existsSync(p)) {
    throw new Error(
      `redprove: no existing golden at ${p} — run \`bun run rebaseline\` first. Red-prove compares against the REAL golden; it has nothing to disprove without one.`
    );
  }
  return readFileSync(p, "utf8");
}

function assertRed(label, gotText) {
  const wanted = existingGolden(label);
  expect(gotText, `redprove FAILED for "${label}": the blank-page extraction matched the real golden — the harness cannot tell a real daemon from an empty one for this lens`).not.toBe(wanted);
}

test.describe.configure({ mode: "serial" });

test("blank page fails every golden comparison (fleet/console/runs/machine)", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1)); // any fixed time — no corpus meta to align to here
  installBlankRoutes(page);

  await page.goto("/index.html");
  // Fleet's `.fleet` wrapper is emitted unconditionally by renderFleet() —
  // present with zero cards just as much as with real ones — so the same
  // selector as extract.spec.ts applies.
  await waitSettled(page, expect, "#stage .fleet");
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  const fleetText = await extractLensText(page);
  assertRed("fleet", fleetText);
  const fleetStageText = await regionText(page, "stage");

  // Console: /panel/mission-status 404s, so renderConsole()'s error branch
  // fires — `.panelerr`, not `.panelout`. The combined selector (same as
  // extract.spec.ts) already covers both.
  await page.click("#lens-console");
  await waitSettled(page, expect, "#stage .panelout, #stage .panelerr", { previousText: fleetStageText });
  const consoleText = await extractLensText(page);
  assertRed("console", consoleText);
  let panelStageText = await regionText(page, "stage");

  // The other six auto-refreshable panels (Packet 6 growth): every
  // `/panel/*` path 404s under installBlankRoutes regardless of id, landing
  // renderConsole()'s error branch (`.panelerr`) every time — there is no
  // "loaded" state reachable here at all, so (unlike extract.spec.ts's
  // `.pc-cmd`-gated wait, needed there specifically to skip past a real
  // async fetch) the simple combined selector + previousText is exact: the
  // chrome's `$ darkmux <id>` line already differs per-id on the very first
  // synchronous render, before the 404 even lands.
  for (const panelId of ["mission-status-all", "machine-status", "flow-status", "role-list", "config-list", "lab-fixture-list"]) {
    await page.click(`[data-act="setpanel"][data-arg="${panelId}"]`);
    await waitSettled(page, expect, "#stage .panelout, #stage .panelerr", { previousText: panelStageText });
    assertRed(`console-${panelId}`, await extractLensText(page));
    panelStageText = await regionText(page, "stage");
  }

  // doctor — MANUAL-ONLY: selecting the tab renders "not yet run" from pure
  // client state (no fetch at all — `setPanel`'s MANUAL_PANELS guard returns
  // before `loadPanel` is ever called), so `#stage` alone is IDENTICAL
  // between a real daemon and a blank one for this one golden. What still
  // makes this a valid red-prove is `#meta`: boot's own live-window fetch
  // 404s under blank routes, so the badge line differs from the real
  // corpus's boot — `assertRed` compares the FULL four-region text
  // (`extractLensText`), not `#stage` alone, so the comparison is still
  // meaningful even though the stage half is deliberately unchanged.
  await page.click('[data-act="setpanel"][data-arg="doctor"]');
  await waitSettled(page, expect, "#stage .panelout, #stage .panelerr", { previousText: panelStageText });
  assertRed("console-doctor-not-run", await extractLensText(page));
  const doctorNotRunStageText = await regionText(page, "stage");

  // Clicking "run" DOES hit the network (404 under blank routes) — same
  // `.panelerr` landing as every other panel above.
  await page.click('[data-act="refreshpanel"]');
  await waitSettled(page, expect, "#stage .panelout, #stage .panelerr", { previousText: doctorNotRunStageText });
  assertRed("console-doctor", await extractLensText(page));
  const consoleStageText = await regionText(page, "stage");

  // Runs: /runs and /lab/runs both 404, but loadRuns()/loadLabRuns()'s catch
  // blocks still set RUNS_LOADED=true (just with empty arrays) — the loading
  // branch (`RUNS_LOADED===null`) is skipped and `.lablist` renders with a
  // "no runs recorded yet" empty state. Same selector as extract.spec.ts.
  await page.click("#lens-runs");
  await waitSettled(page, expect, "#stage .lablist", { previousText: consoleStageText });
  const runsText = await extractLensText(page);
  assertRed("runs", runsText);
  const runsStageText = await regionText(page, "stage");

  await page.click('[data-act="runskind"][data-arg="lab"]');
  await waitSettled(page, expect, '#stage .lablist, [data-act="runskind"][data-arg="lab"].on', { previousText: runsStageText });
  const runsLabText = await extractLensText(page);
  assertRed("runs-kind-lab", runsLabText);
  const runsLabStageText = await regionText(page, "stage");

  // Packet 3 growth: kind=mission / kind=dispatch — /runs 404s, so both
  // filters land on `runsFiltered()`'s empty branch ("no mission/dispatch
  // runs recorded yet."), which still differs from the real (non-empty)
  // golden. Same `.lablist` + chip `.on` settle pattern as kind=lab above.
  await page.click('[data-act="runskind"][data-arg="mission"]');
  await waitSettled(page, expect, '#stage .lablist, [data-act="runskind"][data-arg="mission"].on', { previousText: runsLabStageText });
  const runsMissionText = await extractLensText(page);
  assertRed("runs-kind-mission", runsMissionText);
  const runsMissionStageText = await regionText(page, "stage");

  await page.click('[data-act="runskind"][data-arg="dispatch"]');
  await waitSettled(page, expect, '#stage .lablist, [data-act="runskind"][data-arg="dispatch"].on', { previousText: runsMissionStageText });
  const runsDispatchText = await extractLensText(page);
  assertRed("runs-kind-dispatch", runsDispatchText);
  const runsDispatchStageText = await regionText(page, "stage");

  // ◧ series: /lab/runs 404s too, so `groupLabRunsByTask([])` yields zero
  // groups — `renderLabRunsList()`'s series branch still emits `.lablist`
  // (wrapping the "no lab runs with a recorded corpus yet" empty state, NOT
  // `.labtaskcard` — that class only appears for a real, non-empty group),
  // so the settle marker here is `.lablist` + the toggle's own `.on` state,
  // not `.labtaskcard` (which the blank page can never produce).
  await page.click('[data-act="runskind"][data-arg="lab"]');
  await waitSettled(page, expect, '#stage .lablist, [data-act="runskind"][data-arg="lab"].on', { previousText: runsDispatchStageText });
  const runsLabAgainStageText = await regionText(page, "stage");
  await page.click('[data-act="runsseries"]');
  await waitSettled(page, expect, '#stage .lablist, [data-act="runsseries"].on', { previousText: runsLabAgainStageText });
  const runsSeriesText = await extractLensText(page);
  assertRed("runs-series", runsSeriesText);
  const runsSeriesStageText = await regionText(page, "stage");

  // Machine: /machine/resources 404s, so MACHINE_MEM never populates —
  // renderMachine() permanently lands in `MACHINE_MEM_ERR && !b` branch
  // ("daemon not reachable…"), which ALSO uses class="none" (the same class
  // the "loading…" placeholder uses), so CSS class alone can't tell them
  // apart. Distinguish by text via `.or()`: either the real-data `.memcard`
  // (never reachable here, kept for symmetry) OR a `.none` whose text names
  // the daemon as unreachable (this is the branch that actually fires).
  const machineSettled = page
    .locator("#stage .memcard")
    .or(page.locator("#stage .none", { hasText: /not reachable/i }));
  await page.click("#lens-machine");
  await waitSettled(page, expect, machineSettled, { previousText: runsSeriesStageText });
  const machineText = await extractLensText(page);
  assertRed("machine", machineText);
});

test("blank page fails the #lens=runs deep-link boot golden comparison", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1));
  installBlankRoutes(page);

  await page.goto("/index.html#lens=runs");
  await waitSettled(page, expect, "#stage .lablist");
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  assertRed("runs-lens-boot", await extractLensText(page));
});

test("blank page fails the #lens=machine deep-link golden comparison", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1));
  installBlankRoutes(page);

  // Same settled-marker reasoning as the fleet/machine case above: the blank
  // page's `/machine/resources` 404s, so `renderMachine()` lands in the
  // "daemon not reachable" `.none` branch — never `.memcard`.
  const machineSettled = page
    .locator("#stage .memcard")
    .or(page.locator("#stage .none", { hasText: /not reachable/i }));
  await page.goto("/index.html#lens=machine");
  await waitSettled(page, expect, machineSettled);
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  assertRed("machine-deeplink", await extractLensText(page));
});

test("blank page fails the #session=task-list golden comparison", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1));
  installBlankRoutes(page);

  await page.goto("/index.html#session=task-list");
  // Same as extract.spec.ts: renderSubsystem() is synchronous over
  // already-loaded (here: empty) RAW, no loading-placeholder race.
  await waitSettled(page, expect, "#stage .sub");
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  assertRed("session-task-list", await extractLensText(page));
});

test("blank page fails the catalog-open golden comparison (Packet 4)", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1));
  installBlankRoutes(page);

  await page.goto("/index.html");
  await waitSettled(page, expect, "#stage .fleet");
  await expect(page.locator("body")).not.toHaveClass(/booting/);

  // /flow-days and /flow-missions both 404 here — toggleCatalog()'s
  // Promise.allSettled swallows both failures to empty arrays, so the panel
  // still renders (the "● live · today" row is unconditional — see
  // extract.spec.ts's own comment) but with NEITHER a missions section nor
  // any day rows, just the `.catempty` "no recorded days yet" fallback —
  // structurally nothing like the real corpus's 155-mission/80-day golden.
  await page.click("#srcbadge");
  await waitSettled(page, expect, "#catpanel .catrow");
  assertRed("catalog-open", await extractLensTextWithCatalog(page));
});

test("blank page fails the #mission=<id> golden comparison (Packet 4)", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1));
  installBlankRoutes(page);

  await page.goto("/index.html#mission=acp-ephemeral-pr-ship-1786152707367180000-5");
  await waitSettled(page, expect, "#stage .fleet");
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  // Both the flow window (RAW=[]) AND /missions+/phases 404 here — unlike
  // the real corpus's `mission-replay` golden, where /missions+/phases
  // return real content (155KB+ of real mission/phase data) even though the
  // flow-mission fetch itself ALSO lands on empty RAW on both harnesses (the
  // real corpus answers 200+empty, installBlankRoutes 404s — either way
  // `RAW.length` is 0, see mock-routes.js's own comment on the
  // installCorpusRoutes side). That /missions+/phases difference
  // (savingsHero()'s mission-aware rendering) is what keeps this golden from
  // being a vacuous "both sides are empty" comparison.
  assertRed("mission-replay", await extractLensText(page));
});

test("blank page fails the bare #<date> playback golden comparison (Packet 4)", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1));
  installBlankRoutes(page);

  // Any date string works here (installBlankRoutes 404s every /flow/<date>
  // path uniformly) — the real golden pins the corpus's own previous-day
  // date, but red-prove only needs A bare-date hash to exercise the same
  // code path with everything 404ing.
  await page.goto("/index.html#2026-08-07");
  await waitSettled(page, expect, "#stage .fleet");
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  assertRed("playback-date", await extractLensText(page));
});
