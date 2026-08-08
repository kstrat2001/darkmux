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
import { extractLensText, extractLensTextWithCatalog, waitSettled, installFrozenClock, regionText } from "./lib/extract-lens.js";

mkdirSync(GOLDENS_DIR, { recursive: true });

async function extractAndWrite(page, label) {
  const golden = await extractLensText(page);
  writeFileSync(`${GOLDENS_DIR}/${label}.txt`, golden, "utf8");
  return golden;
}

async function extractAndWriteWithCatalog(page, label) {
  const golden = await extractLensTextWithCatalog(page);
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
  // alone isn't sufficient here.
  //
  // MUST-FIX (QA, post-Packet-6 review): this FIRST console capture used to
  // pair that selector with a `previousText` check — comparing against
  // `fleetStageText` (a completely different lens's leftover text) — on the
  // theory that "any change from a totally different render" was enough to
  // prove settlement. It is NOT: QA red-proved it with 300ms of injected
  // latency on `/panel/mission-status` and the OLD pattern committed a
  // 109-byte "running…" placeholder instead of the real ~1.2KB content,
  // reproduced byte-for-byte here (transcript in the packet report) — the
  // exact same race class as the 2b loop below, just never caught here
  // because the mocked corpus route normally resolves fast enough that the
  // placeholder is never actually observed. Same fix as 2b: wait on
  // `RERUN_BUTTON`, the genuinely exclusive "re-run" marker (only the
  // `if(st.body)` chrome branch renders it; the `else` branch's button
  // always says "run"), defined once here and reused by every console
  // settle-wait below. Red-proved BOTH ways under the same injected latency:
  // the OLD selector fails (captures the placeholder), `RERUN_BUTTON`
  // passes (captures the real content) — see the packet report.
  const RERUN_BUTTON = '.panelchrome button:text-is("re-run")';
  await page.click("#lens-console");
  await waitSettled(page, expect, RERUN_BUTTON, { previousText: fleetStageText });
  await extractAndWrite(page, "console");
  let panelStageText = await regionText(page, "stage");

  // 2b. The other six auto-refreshable console panels (Packet 6 growth —
  // only `mission-status`, the default, had a golden before this). Each tab
  // click re-fetches through `loadPanel()`/`setPanel()`, which paints TWO
  // synchronous intermediate renders before the (mocked, near-instant) fetch
  // resolves — "not yet run" then "running…" — both still `!st.body`. A
  // `previousText`-only settle check is provably racy here: switching
  // panel-to-panel, either intermediate render ALREADY differs from the
  // prior panel's real content, so `waitSettled` can report "settled" on the
  // transient placeholder — caught empirically by `bun run determinism`
  // flagging a genuine two-run mismatch on `console-lab-fixture-list.txt`
  // (618B loaded vs 249B placeholder).
  //
  // FIRST attempt at a fix used `#stage .pc-cmd` as the "loaded" marker and
  // was WRONG — re-reading `renderConsole()` shows `.pc-cmd` is emitted in
  // BOTH chrome branches (`if(st.body){...<span class="pc-cmd">...} else
  // {...<span class="pc-cmd">...}`). QA's own diff (post-review, comparing
  // `argv.join(" ")` against `id.replace(/-/g," ")` for every panel in
  // `panel.rs`'s allowlist) found the text is identical for 7 of the 8 panel
  // ids — `mission-status-all` is the ONE exception (`argv` is `["mission",
  // "status", "--all"]` → `"mission status --all"`, but the id-based guess
  // is `"mission status all"` — the `--all` flag doesn't round-trip through
  // dash-splitting) — so `.pc-cmd` never reliably gated anything beyond the
  // pre-existing `previousText` check, and the race was still latent for the
  // other 7. The ACTUALLY exclusive marker is the "re-run" BUTTON TEXT: only
  // the `if(st.body)` branch's button says "re-run" (the `else` branch's
  // button always says "run", loading or not) — a genuine discriminator
  // with no shared-text trap, for every panel including the exception.
  // Re-verified: `bun run determinism` run 5x back-to-back, byte-identical
  // every time (see the packet report for the transcript).
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

test("#lens=machine deep link (fresh boot straight into the machine lens — Packet 2)", async ({ page }) => {
  // A FRESH boot with the hash already set, unlike the click-navigation path
  // above (`#lens-machine` click from an already-booted fleet lens). This is
  // a genuinely different code path: `boot()`'s `machineQuery()` branch
  // (viewer.html:3791-3796) fires BEFORE the fleet lens ever renders, so the
  // page never passes through `renderFleet()` at all — a bug in that
  // precedence check (e.g. the fleet lens winning the race) would only show
  // up on this path, not the click path. See tests/parity/README.md's
  // "Deep-link boot paths" coverage-gap note — this closes the `#lens=
  // machine` gap named there for Packet 2.
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.goto("/index.html#lens=machine");
  await waitSettled(page, expect, "#stage .memcard");
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  await extractAndWrite(page, "machine-deeplink");
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

test("catalog picker (#catpanel): live row + capped missions + all days from the corpus (Packet 4)", async ({ page }) => {
  // The catalog panel is global chrome (#691's day/mission history browser),
  // reachable from any lens once boot() wires `data-act=catalog` onto
  // `#srcbadge` (see viewer.html's boot(): `if(!flowSrc && mode!=="no-daemon")
  // {...sb.dataset.act="catalog";...}`) — a fresh boot straight to the
  // default fleet lens is how an operator actually reaches it ("browse
  // history" from wherever they land), matching the README's own KNOWN
  // COVERAGE GAPS note this test closes: "the extractor doesn't capture it
  // at all; it's a modal overlay, not part of #stage".
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.goto("/index.html");
  await waitSettled(page, expect, "#stage .fleet");
  await expect(page.locator("body")).not.toHaveClass(/booting/);

  // toggleCatalog() paints a synchronous "loading…" placeholder (a single
  // `.cathdr`, no `.catrow`) before its two fetches (flow-days +
  // flow-missions, Promise.allSettled) resolve — `.catrow` never appears in
  // that placeholder, only once real rows render (the "● live · today" row
  // is unconditional, so it alone is a safe post-fetch marker even on a
  // corpus with zero days/missions).
  await page.click("#srcbadge");
  await waitSettled(page, expect, "#catpanel .catrow");
  // This corpus has 155 missions (> CATALOG_MISSION_CAP=50) and 80 days —
  // the golden below is what exercises the "newest 50 of 155" cap-disclosure
  // header (#1569 sweep) for real, not a fabricated small fixture.
  await extractAndWriteWithCatalog(page, "catalog-open");
});

test("#mission=<known-corpus-mission> deep link (Packet 4 — the unknown-id in-page path)", async ({ page }) => {
  // A FRESH boot, same reasoning as the #session=task-list test above:
  // catalogQuery() only resolves at boot time.
  //
  // missionGraphReachable() is TRUE in this harness (darkmux-mode=live, no
  // darkmux-flow-src — see playwright.config.js's own comment), so a
  // POPULATED /flow-mission/<id> response would navigate boot() straight to
  // /mission/<id>/graph (viewer.html's boot(): `if(cq&&cq.kind==="mission"&&
  // RAW.length){location.href=...;return;}`) — a separate document this
  // harness's static file server can't serve and the README's lens inventory
  // already named as out of scope for this suite. `lib/mock-routes.js`
  // answers EVERY `/flow-mission/:id` path with an honest 200+empty payload
  // (`{records:[],count:0,...}` — matching what the REAL endpoint returns
  // for an unmatched id; see that file's own comment for why this was
  // changed from Packet 0a's original 404-everything mock, and confirmed via
  // `bun run check` that the change is byte-invisible to every legacy
  // golden), regardless of which id is named — so RAW stays empty, the
  // navigation guard's `RAW.length` check is false, and boot() falls through
  // to its normal `render()` call instead. This records what legacy
  // ACTUALLY does on THIS corpus: an empty in-page fleet render, not the
  // graph page — the brief's own "record what legacy actually does,
  // honestly" instruction, not a stand-in for the populated/navigates-away
  // case (which needs real per-mission record fixtures this corpus doesn't
  // have — see README).
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.goto("/index.html#mission=acp-ephemeral-pr-ship-1786152707367180000-5");
  await waitSettled(page, expect, "#stage .fleet");
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  // Confirm boot() really did NOT navigate away — the page is still on
  // index.html, not `/mission/<id>/graph` (which this harness's static file
  // server would 404 on, a different failure mode than what this test means
  // to prove).
  expect(new URL(page.url()).pathname).toBe("/index.html");
  await extractAndWrite(page, "mission-replay");
});

test("bare #<date> hash — playback for a non-today day (Packet 4 — flips `live` false)", async ({ page }) => {
  // A FRESH boot at a bare date hash — targetDate()'s fallback convenience
  // (type/bookmark a date directly), distinct from the catalog panel's OWN
  // day-row click (`location.href="/play/"+date`, a full navigation to a
  // SEPARATE server route this harness doesn't serve). This is the
  // README's other named KNOWN COVERAGE GAP: "a bare #<date> hash
  // (playback-by-date, daemon-only)". The corpus's OWN previous-day date
  // (meta.captured_prev_date, backed by the real flow-yesterday.json fixture
  // — 1993 records) is used so the render has genuine content, not an empty
  // shell — and, being genuinely NOT today, `boot()`'s `live = !wantsPlayback
  // && date===todayUTC()` check is false, landing on the playback fetch
  // branch (`/flow/<date>`) instead of the live-window branch — a real,
  // distinct render path from the default `fleet.txt` golden.
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.goto(`/index.html#${meta.captured_prev_date}`);
  await waitSettled(page, expect, "#stage .fleet");
  await expect(page.locator("body")).not.toHaveClass(/booting/);
  await extractAndWrite(page, "playback-date");
});
