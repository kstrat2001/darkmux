// @ts-nocheck
// Golden extraction — Packet 0a (the parity harness). Serves the repo's real
// crates/darkmux-serve/assets/viewer.html (via playwright.config.js's
// live-mode harness), intercepts every network call with corpus fixtures
// recorded from the operator's live daemon (`bun run record` first), freezes
// the clock to the corpus's capture time, and walks every lens in the
// inventory below — extracting normalized text from each into
// goldens/<lens>.txt.
//
// This file is the REGENERATION path (`bun run rebaseline` / `bun run
// extract`) — it always overwrites goldens/. `bun run check` does NOT call
// this file directly; it calls `verify-goldens.mjs`, which snapshots
// goldens/ first, runs this same extraction, diffs the result, and restores
// the snapshot on any mismatch (see that file's module doc for why —
// running this file unconditionally inside `check` is exactly what let a
// hand-corrupted golden sail through: extraction silently overwrote the
// corruption before anything downstream could see it).
//
// This is the SPECIFICATION half of the parity harness (see redprove.spec.ts
// for the half that proves it can fail — it imports extractLensText/
// waitSettled from the SAME lib/extract-lens.js this file uses, so the two
// can't quietly drift apart). Goldens change ONLY deliberately, in a
// reviewed diff — see README.md.

import { test, expect } from "@playwright/test";
import { mkdirSync, writeFileSync } from "node:fs";
import { GOLDENS_DIR } from "./lib/paths.js";
import { loadMeta, installCorpusRoutes } from "./lib/mock-routes.js";
import { extractLensText, waitSettled, installFrozenClock, regionText } from "./lib/extract-lens.js";

mkdirSync(GOLDENS_DIR, { recursive: true });

async function extractAndWrite(page, label) {
  const golden = await extractLensText(page);
  writeFileSync(`${GOLDENS_DIR}/${label}.txt`, golden, "utf8");
  return golden;
}

test.describe.configure({ mode: "serial" });

test("boot + four lens tabs (fleet, console, runs, machine)", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.goto("/index.html");
  // Boot's own post-fetch content marker: the fleet lens is the default
  // landing state, and renderFleet() only ever writes `.fleet` once the
  // live-window + missions/phases loads have both resolved (see boot()'s
  // sequencing in viewer.html — render() is the LAST call, after every
  // await). No loading-placeholder branch to race here.
  await waitSettled(page, expect, "#stage .fleet");
  await expect(page.locator("body")).not.toHaveClass(/booting/);

  // 1. fleet — the default landing lens.
  await extractAndWrite(page, "fleet");
  const fleetStageText = await regionText(page, "stage");

  // 2. console — the CLI-panel lens. Default panel is mission-status.
  // renderConsole()'s loading/error/loaded branches are mutually exclusive
  // on `.panelout`/`.panelerr`, and only the loaded branch's `.panelout`
  // carries the real `$ darkmux mission status` output — the loading branch
  // ALSO emits `.panelout` (with literal "running…" text), so the selector
  // alone isn't sufficient here; pair it with the previousText check, which
  // catches "still showing the placeholder" because that placeholder text
  // differs from the fleet lens's leftover text too, but more importantly a
  // POLL against the placeholder's own STABLE text would otherwise pass
  // immediately — see the redprove/latency-injection proof in the report for
  // why this combination (not either check alone) is what closes the gap.
  await page.click("#lens-console");
  await waitSettled(page, expect, "#stage .panelout, #stage .panelerr", { previousText: fleetStageText });
  await extractAndWrite(page, "console");
  let panelStageText = await regionText(page, "stage");

  // 2b. The other six auto-refreshable console panels (Packet 6 growth —
  // only `mission-status`, the default, had a golden before this). Each tab
  // click re-fetches through `loadPanel()`/`setPanel()`, which paints TWO
  // synchronous intermediate renders before the (mocked, near-instant) fetch
  // resolves — "not yet run" then "running…" — both still `!st.body`. A
  // `previousText`-only settle check (as used for the very first console
  // click above, landing from a completely different lens) is provably racy
  // here: switching panel-to-panel, either intermediate render ALREADY
  // differs from the prior panel's real content, so `waitSettled` can report
  // "settled" on the transient placeholder — caught empirically by
  // `bun run determinism` flagging a genuine two-run mismatch on
  // `console-lab-fixture-list.txt` (618B loaded vs 249B placeholder).
  //
  // FIRST attempt at a fix used `#stage .pc-cmd` as the "loaded" marker and
  // was WRONG — re-reading `renderConsole()` shows `.pc-cmd` is emitted in
  // BOTH chrome branches (`if(st.body){...<span class="pc-cmd">...} else
  // {...<span class="pc-cmd">...}`), with IDENTICAL text for these panel ids
  // besides (`argv.join(" ")` and `id.replace(/-/g," ")` produce the same
  // string for every id here) — so waiting on it never actually gated
  // anything beyond the pre-existing `previousText` check, and the race was
  // still latent. The ACTUALLY exclusive marker is the "re-run" BUTTON TEXT:
  // only the `if(st.body)` branch's button says "re-run" (the `else`
  // branch's button always says "run", loading or not) — a genuine
  // discriminator with no shared-text trap. Re-verified: `bun run
  // determinism` run 5x back-to-back, byte-identical every time (see the
  // packet report for the transcript).
  const RERUN_BUTTON = '.panelchrome button:text-is("re-run")';
  for (const panelId of ["mission-status-all", "machine-status", "flow-status", "role-list", "config-list", "lab-fixture-list"]) {
    await page.click(`[data-act="setpanel"][data-arg="${panelId}"]`);
    await waitSettled(page, expect, RERUN_BUTTON, { previousText: panelStageText });
    await extractAndWrite(page, `console-${panelId}`);
    panelStageText = await regionText(page, "stage");
  }

  // 2c. doctor — MANUAL-ONLY (#1286): selecting the tab must NOT auto-fetch.
  // Capture the "not yet run" placeholder state first (this IS the real,
  // permanent behavior on tab-select — `setPanel`'s MANUAL_PANELS guard
  // returns before ever calling `loadPanel`, so there is no async race to
  // settle here at all, `.panelout`+previousText is exact), then click the
  // panel's own "run" button (`data-act="refreshpanel"`, the same affordance
  // an operator would use) to capture the real probed content — same
  // `RERUN_BUTTON` marker as the loop above, for the same reason.
  await page.click('[data-act="setpanel"][data-arg="doctor"]');
  await waitSettled(page, expect, "#stage .panelout, #stage .panelerr", { previousText: panelStageText });
  await extractAndWrite(page, "console-doctor-not-run");
  const doctorNotRunStageText = await regionText(page, "stage");

  await page.click('[data-act="refreshpanel"]');
  await waitSettled(page, expect, RERUN_BUTTON, { previousText: doctorNotRunStageText });
  await extractAndWrite(page, "console-doctor");
  const consoleStageText = await regionText(page, "stage");

  // 3. runs — the consolidated kind-tagged run list, default kind=all.
  // `.lablist` is the loaded-content wrapper (present for BOTH the flat
  // list and the lab-series view) — renderLabRunsList()'s `RUNS_LOADED===null`
  // loading branch never emits it, only a bare `.none`/"loading…" placeholder.
  await page.click("#lens-runs");
  await waitSettled(page, expect, "#stage .lablist", { previousText: consoleStageText });
  await extractAndWrite(page, "runs");
  const runsStageText = await regionText(page, "stage");

  // 3b. runs, kind=lab — a genuinely different render (the series/knob-diff
  // view over LAB_RUNS) reached by re-filtering the same loaded list
  // client-side (no new fetch), so it's cheap to capture as a bonus golden.
  // Same `.lablist` marker; the kind chip's `.on` state confirms the filter
  // actually switched (defense against a click that silently no-opped).
  await page.click('[data-act="runskind"][data-arg="lab"]');
  await waitSettled(page, expect, '#stage .lablist, [data-act="runskind"][data-arg="lab"].on', { previousText: runsStageText });
  await extractAndWrite(page, "runs-kind-lab");
  const runsLabStageText = await regionText(page, "stage");

  // 3c. runs, kind=mission — the other two filter chips (Packet 3 growth:
  // only kind=all/kind=lab were golden-tested before this). Same re-filter,
  // no new fetch — `.lablist` + the chip's `.on` state is the same settle
  // pattern as 3b.
  await page.click('[data-act="runskind"][data-arg="mission"]');
  await waitSettled(page, expect, '#stage .lablist, [data-act="runskind"][data-arg="mission"].on', { previousText: runsLabStageText });
  await extractAndWrite(page, "runs-kind-mission");
  const runsMissionStageText = await regionText(page, "stage");

  // 3d. runs, kind=dispatch — the last uncovered chip.
  await page.click('[data-act="runskind"][data-arg="dispatch"]');
  await waitSettled(page, expect, '#stage .lablist, [data-act="runskind"][data-arg="dispatch"].on', { previousText: runsMissionStageText });
  await extractAndWrite(page, "runs-kind-dispatch");
  const runsDispatchStageText = await regionText(page, "stage");

  // 3e. runs, kind=lab, ◧ series — the series/knob-diff sub-view
  // (`state.runsSeries===true`), reached only from under the lab filter. It
  // is the ONE thing `/lab/runs` actually feeds (see README's correction);
  // this golden is what makes that fixture live rather than recorded-but-
  // inert. `.labtaskcard` is `renderLabTaskCard`'s wrapper — present only in
  // the series view, never in the flat `.labrunrow` list — so it alone
  // proves the toggle actually rendered the grouped view, not just flipped a
  // CSS class on the same rows.
  await page.click('[data-act="runskind"][data-arg="lab"]');
  await waitSettled(page, expect, '#stage .lablist, [data-act="runskind"][data-arg="lab"].on', { previousText: runsDispatchStageText });
  const runsLabAgainStageText = await regionText(page, "stage");
  await page.click('[data-act="runsseries"]');
  await waitSettled(page, expect, '#stage .lablist, [data-act="runsseries"].on', { previousText: runsLabAgainStageText });
  await extractAndWrite(page, "runs-series");
  const runsSeriesStageText = await regionText(page, "stage");

  // 4. machine — the unified local-machine page (#lens=machine). `.memcard`
  // is the loaded-content marker for the residency/RAM section — absent
  // during goMachine()'s `!b` ("loading…") branch in renderMachine().
  await page.click("#lens-machine");
  await waitSettled(page, expect, "#stage .memcard", { previousText: runsSeriesStageText });
  await extractAndWrite(page, "machine");
});

test("#lens=runs deep-link boot (boot()'s lq path, not a click-through)", async ({ page }) => {
  // A FRESH boot straight at `#lens=runs` (default kind=all) — exercises
  // `boot()`'s own `lq` deep-link branch (`state.runsKind=lq.kind||"all";
  // goRuns()`, called BEFORE the first `render()` ever paints fleet — see
  // viewer.html's boot()), which is a genuinely different code path than
  // clicking the runs tab after landing on fleet (that path goes through
  // `window.setRunsKind`/the `data-act="runs"` delegate, never through
  // `lq`). Content is expected to match `runs.txt` byte-for-byte (same
  // corpus, same default kind=all) — the golden's value is proving the BOOT
  // MECHANISM independently, not a different render.
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.goto("/index.html#lens=runs");
  await waitSettled(page, expect, "#stage .lablist");
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  await extractAndWrite(page, "runs-lens-boot");
});

test("#session=task-list deep link (drill-in rendered inside viewer.html)", async ({ page }) => {
  // A FRESH boot, because the session catalog query only resolves at boot
  // time (catalogQuery() -> the cq branch in boot()) — see viewer.html's own
  // comment: "a URL carrying both is contradictory, and the explicit lens
  // wins", and drillSession() itself does no fetching of its own, it only
  // re-scopes the already-loaded RAW records.
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.goto("/index.html#session=task-list");
  // `.sub` is renderSubsystem()'s wrapper — synchronous (runRegions() reads
  // already-loaded RAW, no fetch of its own), so there's no loading-
  // placeholder race here, but the marker is asserted anyway for symmetry
  // and because a future refactor could add one.
  await waitSettled(page, expect, "#stage .sub");
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  await extractAndWrite(page, "session-task-list");
});
