// @ts-nocheck
// The NEW-UI ACCEPTANCE GATE (Packet 2 — the machine lens, the first real
// lens ported onto the React + TanStack Query scaffold). Loads the
// COMMITTED BUILT `/next` artifact (`crates/darkmux-serve/assets/next.html`,
// served via `next-parity.playwright.config.js`'s dedicated webServer) the
// same way the legacy extractor (`extract.spec.ts`, retired #1806) once
// loaded the legacy `viewer.html`: the SAME sanitized corpus fixtures via
// `page.route()` interception (`installCorpusRoutes`, `lib/mock-routes.js`
// — shared verbatim with the retired legacy extractor, not a lookalike
// reimplementation), the SAME frozen clock, the SAME extraction logic
// (`lib/extract-lens.js`'s `extractLensText`, also shared verbatim). Then
// it asserts the result against `goldens/machine.txt` /
// `goldens/machine-deeplink.txt` — the frozen spec the (now-retired)
// legacy extractor originally recorded from the legacy viewer.
//
// This is deliberately the FIRST file of its kind — later lens packets
// (missions/runs, catalog/replay, console panels, lab) ADD their own
// `test(...)` blocks to this same file rather than inventing a parallel
// harness; the config/route-interception/extraction machinery above is
// the reusable part, not per-lens.
//
// Byte equality is the default (no normalization beyond what
// `extractLensText` already applies for BOTH sides). If a future lens hits
// a fragment that is legitimately unreachable in the new UI (a genuine
// legacy-rendering artifact, not a port bug), the fix is a NAMED
// normalization with a comment justifying it here — never a silent fuzzy
// match.

import { test, expect } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { GOLDENS_DIR, CORPUS_DIR } from "./lib/paths.js";
import { loadMeta, installCorpusRoutes, installBlankRoutes } from "./lib/mock-routes.js";
import { extractLensText, waitSettled, installFrozenClock, regionText, normalize } from "./lib/extract-lens.js";

// Overnight-runbook render-sanity contract (every UI packet's standing
// requirement): zero pageerror, `#stage` visible with real height, no
// horizontal document scroll at phone width. Gallery PNGs land here for the
// operator's morning review (not committed — binary bloat, see the runbook).
//
// Gitignored, repo-relative by default (`tests/parity/.gallery/2-machine/`)
// — NOT an operator machine path (QA must-fix, 2026-08-09: a committed
// absolute path bakes one machine's home-directory layout, and worse a
// session-scoped scratch UUID, into a PUBLIC repo — no gate catches it,
// `tripwire.mjs` only scans `corpus/`+`goldens/`). Every lens packet that
// appends `test()` blocks to this shared file inherits this default, so the
// path propagates correctly by construction rather than by each packet
// remembering to get it right. Override with `DARKMUX_GALLERY_DIR` for a
// real run (e.g. the operator's own scratchpad). `mkdirSync` itself lives
// in the top-level `test.beforeAll` below (fires only when this suite
// actually RUNS, never at import/collection time), matching the sibling
// `next-parity-runs.spec.ts`'s pattern.
const GALLERY_DIR = process.env.DARKMUX_GALLERY_DIR || path.join(__dirname, ".gallery", "2-machine");

test.beforeAll(() => {
  mkdirSync(GALLERY_DIR, { recursive: true });
});

function screenshotPath(name) {
  return path.join(GALLERY_DIR, name);
}

function readGolden(label) {
  return readFileSync(`${GOLDENS_DIR}/${label}.txt`, "utf8");
}

/**
 * (#1809, finishing #1508 step 4) `MachineLens` deliberately stopped
 * matching `goldens/machine.txt`/`goldens/machine-deeplink.txt` byte-for-
 * byte the moment its `RUNS ON <MACHINE>` list moved out to the runs lens
 * (`#lens=runs&machine=<uid>`) — 80 of the golden's 131 lines ARE that list
 * (`RUNS ON MACBOOK-PRO` through `show all 30 →`), and this port no longer
 * renders it at all, replaced by a "N runs on <machine> →" link with no
 * golden counterpart. This is NOT legacy behaving wrongly, and it is NOT a
 * regression to chase back to byte parity: #1508 step 2's own commit
 * (`d2041ae3`) named the list "deliberately interim", and #1809 is the
 * step-4 follow-up that finishes moving it out — see `MachineLens.tsx`'s
 * own module doc for the full history.
 *
 * The golden itself is NOT edited (goldens are the frozen record of what
 * LEGACY did; legacy was never wrong here, the port just moved on) and the
 * assertion is NOT deleted (see `MachineLens.tsx`'s doc + this repo's own
 * `tests/parity/README.md` for why an un-asserted lens is worse than a
 * narrowed one). It is NARROWED, the same way this file's sibling
 * (`next-parity-catalog.spec.ts`) narrows its own `mission-replay` case —
 * name exactly which region still corresponds and why, keep everything
 * else at full byte equality.
 *
 * (#1806 Stage 2/3 — the machine-lens redesign, docs/design/machine-lens/proposal.md in the design
 * packet) #1809 narrowed the comparison to `topbar`/`crumb`/`meta`/
 * `logscope` (`machineChromePrefixOf`, still full byte equality below)
 * PLUS the stage's header + (at the time) the `darkmux/utility` block + the health/pressure
 * LEDGER text. Stage 2/3 is the second, deeper narrowing: it retires the
 * ledger half of that comparison entirely.
 *
 * **What diverged.** The ledger used to be one flat, classified string per
 * fact (`Σ potential 29.83 GB · Σ current 22.21 GB · limit …`, `darkmux:
 * qwen3.6-35b … GREEN … ctx 262144 · weights 18.45 GB …`) — an `innerText`
 * walk of that region was a meaningful byte-exact spec because the PORT'S
 * OWN structure was still "one line per fact", the same shape legacy's
 * flattened extraction produced. Stage 2/3 (docs/design/machine-lens/proposal.md, operator-approved)
 * replaced that shape on purpose: a bezel-less semicircle gauge whose
 * reading is a NEEDLE POSITION plus a handful of on-arc tick labels
 * (`0 · 34 · 69 · 103 · 137 · LIMIT` as of this writing — #1811 has since
 * moved the arc to binary, `0 · 32 · 64 · 96 · 128`, which changes nothing
 * about the argument here), a tell-tale lamp row that renders
 * SEVEN lamps every time regardless of payload (`STATE GREEN`, `Δ
 * RESIDENCY`, `UNPRICED`, …), odometer DIGIT CELLS for the pressure
 * instruments (`8`/`8` rather than `88%`), and model ROWS with ghost/NEW
 * residency chips that don't exist in the flat ledger's vocabulary at all.
 * None of that is a bug — it is the redesign's entire point (docs/design/machine-lens/proposal.md
 * §1's whole argument is that the flat ledger was the defect) — but it
 * means an `innerText` diff against a golden recorded from the FLAT shape
 * is comparing two genuinely different information architectures, not
 * catching drift within one. The failing diff is representative: every
 * line differs, because every line's SHAPE differs, not because any single
 * fact stopped being reported.
 *
 * **Why this is deliberate, not a silently-lost regression.** #1806 Stage 1
 * already established the precedent this follows (this file's own #1809
 * narrowing) — a redesign that changes RENDERING while explicitly PRESERVING
 * the underlying facts (every figure the flat ledger reported still renders
 * somewhere in the gauge/lamp/odometer/row cluster — see docs/design/machine-lens/provenance.md's
 * value-by-value trace, written and verified against a live daemon for
 * exactly this packet) is a text-shape change, not an information loss.
 * The operator reviewed and approved the redesign (docs/design/machine-lens/proposal.md, "the
 * chosen shape") BEFORE this narrowing was written, so this is not a
 * unilateral test-weakening — it is the golden format catching up to a
 * decision already made above the test file.
 *
 * **Where the ledger's coverage lives now**, since a byte-diff can no
 * longer be it:
 * - `ui/src/lenses/machine/machineGauge.test.ts` — 46 unit tests on the
 *   pure gauge/lamp/odometer/residency math (no DOM): scale resolution,
 *   needle-angle formula, commit-tick clamping vs overcommit, the redline's
 *   one-field provenance (`machine.state === "red"`, nothing else), every
 *   lamp's single-field key, the ghost/NEW residency state machine
 *   (arrival, departure, one-more-poll-then-retire, reappearance-as-NEW),
 *   and stable darkmux-first/alphabetical sort. RED-PROVED live in this
 *   packet: temporarily changing `redlineLit()` to key on `"amber"` instead
 *   of `"red"` failed 3 tests for the exact right reason (the redline lit
 *   on the wrong state, and the face caption flipped when it shouldn't
 *   have) before the change was reverted — the coverage is not decorative.
 * - `ui/src/lenses/machine/MachineHealthRegion.test.tsx` — 20 component
 *   tests on the assembled region: absence-vs-zero (no `.mm-row-pot`/no
 *   `.mm-gauge-commit` when unpriced/zero, the inverted priced case DOES
 *   draw it), a hostile state string degrading to the neutral class (never
 *   landing raw in a `className`), the redline's state-gate (lit on red,
 *   NOT on amber even at high current, NEVER on a stale/cached read), and
 *   — the #1812 fix this same packet ships — the last-good payload staying
 *   on screen under a visible stale banner rather than being discarded.
 * - `tests/e2e/viewer-machine.spec.js` — 3 real-browser tests: every
 *   hostile string in a realistic `/machine/resources` payload (model
 *   identifiers, shrink hints, attribution note, warnings, a hostile
 *   `state` value) still renders INERTLY through the new markup, the
 *   observer-cost stamp (`#memstamp`) is visible, and (the #1812 regression
 *   test, un-fixme'd in this same packet) an unreachable daemon shows the
 *   no-daemon notice, then a stale banner with the LAST GOOD reading still
 *   on screen once data has existed.
 *
 * 69 tests total replace what this narrowing removes, none of them a byte
 * comparison against a frozen string — each asserts a STRUCTURAL or
 * NUMERIC claim about the new shape instead, which is the right unit for a
 * redesign whose whole point was to change the shape.
 *
 * **Narrowed a THIRD time** (operator-approved `darkmux/utility` block
 * redesign, then its reorder, then its DELETION): the previous narrowing
 * still asserted the header line PLUS that block's fixed four-line shape
 * (`utilityLines()`) byte-for-byte, on the theory the block was untouched by
 * Stage 2/3. It is not untouched anymore — it is GONE. The operator's call
 * after reading it live: the block was CONFIG, not machine state. It named
 * what the utility tier is responsible for, not how it relates to this
 * machine, and this page shows what is resident.
 *
 * Its four states did not need re-homing, which is the test that the cut was
 * right rather than merely tidy: `resident` was already proven by the ledger
 * row's own existence (a row exists iff `lms ps` lists the model), `not
 * loaded` and `not configured` are config questions `darkmux doctor`'s
 * `check_utility_model_binding` already answers WITH a fix hint, and `not
 * reported` duplicated the page-level not-local placeholder. What survives
 * is one neutral `utility` badge on the residency row that renders anyway.
 *
 * What survived THIS byte comparison, narrower still than the prior cut, was
 * for a time ONLY the stage's header line (`.machine-lens__hdr` — "fleet ›
 * machine · <label> — <spec>") — that line is real chrome untouched by any
 * of it (it renders before `.machine-lens__health` and always has), so it
 * stayed a meaningful byte-exact tie, but everything below it went
 * unasserted. #2826 (see `goldenMachineStageText`/`machineStageText` below)
 * RE-WIDENED the comparison back to the full stage, against a fresh capture
 * of the current body rather than the old frozen shape — the header line is
 * still the first line of that comparison, so it stays covered the same way.
 *
 * **Where the coverage lives now**, since a byte-diff can no longer be it:
 * `memoryLedgerLines.test.ts`'s `utilityModelId` describe block (the id
 * lookup and its locality guard), `machineGauge.test.ts`'s
 * `isUtilityTierRow` block (the two-field match mirroring the server, both
 * inverted cases), `MachineHealthRegion.test.tsx`'s "the utility row-chip"
 * block (the rendered badge, its neutral/unclassed severity, and no badge
 * when nothing matches), and `MachineLens.test.tsx`'s "the utility tier is a
 * row badge, not a card" block — which asserts the CARD'S ABSENCE, so it
 * cannot quietly come back, and proves the specs-id → health-region → badge
 * seam end-to-end. Every one of those was red-proved by mutation.
 */
function machineChromePrefixOf(fullText: string): string {
  const stageMarker = "=== stage ===\n";
  const idx = fullText.indexOf(stageMarker);
  if (idx === -1) throw new Error(`machineChromePrefixOf: no "${stageMarker.trim()}" marker found`);
  return normalizeMachineCrumb(fullText.slice(0, idx));
}

/**
 * (operator finding, phone screenshot) A NAMED normalization, per this
 * file's own top-of-file discipline ("Byte equality is the default... the
 * fix is a NAMED normalization with a comment justifying it here — never a
 * silent fuzzy match").
 *
 * `#crumb` on the machine lens used to repeat the machine name
 * (`targetMachineName ?? "this machine"`, matching legacy's `renderCrumb()`
 * byte-for-byte — this is why `goldens/machine.txt`'s own `=== crumb ===`
 * section still reads "MacBook-Pro", a frozen LEGACY artifact never edited
 * out of the golden). `App.tsx` kept that text but folded the element into
 * the sticky tab row on desktop; on a phone the narrow stylesheet gave the
 * SAME in-DOM element its own full-width row regardless of its text, so the
 * machine name rendered as a whole standalone line between the tab bar and
 * `MachineLens`'s own `.machine-lens__hdr` breadcrumb, which had already
 * dropped its OWN copy of the name for the identical reason (see that
 * component's own doc). Root cause was DOM PRESENCE, not text content — so
 * `App.tsx` now doesn't render `<header id="crumb">` AT ALL on the machine
 * route, at any width. `#crumb` extracts as `(empty)` on this lens now,
 * genuinely (not just normalized-away): this function is applied to BOTH
 * sides of the comparison below, so the golden's literal "MacBook-Pro" is
 * replaced with the SAME placeholder the port's real empty crumb already
 * produces, and the surrounding topbar/meta/logscope stay byte-exact. If a
 * future regression brings the name back OR removes it from somewhere it
 * should still appear, this normalization does not hide that — it only
 * neutralizes the one, understood, deliberate divergence named here.
 */
function normalizeMachineCrumb(chromeText: string): string {
  const crumbMarker = "=== crumb ===\n";
  const metaMarker = "=== meta ===\n";
  const crumbIdx = chromeText.indexOf(crumbMarker);
  if (crumbIdx === -1) throw new Error(`normalizeMachineCrumb: no "${crumbMarker.trim()}" marker found`);
  const contentStart = crumbIdx + crumbMarker.length;
  const metaIdx = chromeText.indexOf(metaMarker, contentStart);
  if (metaIdx === -1) throw new Error(`normalizeMachineCrumb: no "${metaMarker.trim()}" marker found after crumb`);
  return chromeText.slice(0, contentStart) + "(empty)\n" + chromeText.slice(metaIdx);
}

/**
 * (#2826 — RE-WIDENED, not narrowed a fifth time) Every prior narrowing
 * documented above (this section's own doc, up through "**Narrowed a THIRD
 * time**") was a response to the port's rendering genuinely diverging from
 * `goldens/machine.txt`'s frozen LEGACY shape — a real redesign, each time.
 * The narrowing left only the stage's header line under byte comparison
 * (`goldenMachineHdrText`/`machineHdrText`, the functions this replaces),
 * with the doc's own words: "everything below it ... was deliberately
 * redesigned" — true, but it meant the BODY had no regression gate at all
 * (#2826's finding).
 *
 * This does not resurrect byte-for-byte comparison against the OLD frozen
 * shape (that would just reintroduce the four-narrowings problem in
 * reverse). Instead, `goldens/machine.txt` / `goldens/machine-deeplink.txt`
 * had their `=== stage ===` section REPLACED with a fresh capture of the
 * CURRENT port's real rendering (`tests/parity/corpus/machine-resources.json`
 * refreshed from a live daemon via `bun run record.mjs --only machine`,
 * #2826 — the corpus previously had no `load` key at all, so the gauges'
 * CPU/GPU/MEM/thermal/power body this golden now covers could not have been
 * driven even if asserted). The `=== topbar ===`/`=== crumb ===`/
 * `=== meta ===`/`=== logscope ===` sections above `=== stage ===` are
 * UNTOUCHED — still the frozen legacy chrome, still compared via
 * `machineChromePrefixOf` exactly as before.
 *
 * Two things the body-body capture surfaced that are NOT covered by this
 * golden today, recorded here rather than silently: (1) `load.battery_health`
 * and `load.now.battery` (charge/on-AC/health/condition) are present in the
 * corpus but rendered NOWHERE in the current machine lens or anywhere else
 * in `ui/src` (verified: zero matches for "battery" in non-test source) —
 * so this golden cannot gate battery regressions until that UI work lands;
 * (2) the `RUNS ON <MACHINE>` list this golden's legacy chrome-adjacent
 * region used to carry is gone by design (#1809, see the doc above), so it
 * has no counterpart in the new stage capture either — the current
 * `runs on <machine> →` link IS captured, at the tail of the stage.
 */
function goldenMachineStageText(goldenText: string): string {
  const stageMarker = "=== stage ===\n";
  const stageIdx = goldenText.indexOf(stageMarker);
  if (stageIdx === -1) throw new Error(`goldenMachineStageText: no "${stageMarker.trim()}" marker found`);
  return normalize(goldenText.slice(stageIdx + stageMarker.length));
}

async function machineStageText(page): Promise<string> {
  const got = await regionText(page, "stage");
  return normalize(got || "(empty)");
}

// NOTE: deliberately NOT `test.describe.configure({ mode: "serial" })` (QA
// take, mutation-proved 2026-08-09) — serial mode meant one lens's failure
// suppressed every OTHER lens's result in this shared file (observed:
// 1 failed / 5 did not run, vs the correct 2 failed / 4 passed once
// removed). Every test here reads its own goldens and writes its own
// distinct screenshot; nothing needs cross-test ordering.

test("next: click-navigation into #lens=machine matches goldens/machine.txt", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  // Boot at the default route first (no hash) — the React-port equivalent
  // of the legacy click-navigation path. This scaffold has no lens-nav-tab
  // UI yet (Packet 1 built only the FleetStrip proof region; a clickable
  // nav is a scaffold gap ledgered in the runbook, not this packet's job to
  // build) — setting `location.hash` is the operator-navigation ACTION the
  // click would otherwise trigger, exercised through `useHashRoute`'s
  // `hashchange` listener rather than a `.click()` on a tab that doesn't
  // exist yet. The distinction that actually matters for this golden pair
  // (click-transition vs fresh-boot — see the deep-link test below) is
  // preserved: this page has ALREADY booted once before the hash changes.
  await page.goto("/index.html");
  await page.evaluate(() => {
    location.hash = "#lens=machine";
  });
  await waitSettled(page, expect, '.machine-lens__health[data-state="loaded"]');
  await expect(page.locator("body")).not.toHaveClass(/booting/);

  // (#1809, then narrowed further by #1806 Stage 2/3, then narrowed a THIRD
  // time by the utility-block redesign + its reorder follow-up, then a
  // FOURTH time — operator finding, phone screenshot — for `#crumb` itself,
  // then RE-WIDENED (#2826) to cover the full stage body again, against a
  // fresh capture rather than the old frozen shape. See this file's own
  // `machineChromePrefixOf`/`normalizeMachineCrumb`/`goldenMachineStageText`
  // doc for exactly which regions no longer correspond and why each is a
  // deliberate divergence, not a regression.
  const got = await extractLensText(page);
  const golden = readGolden("machine");
  // Prove `#crumb` is genuinely empty on THIS side before normalizing it
  // away below — `normalizeMachineCrumb` applies the same placeholder to
  // both sides, which would silently mask a real (non-"MacBook-Pro")
  // regression in the got side if this weren't checked directly first.
  expect(await regionText(page, "crumb"), "#crumb must not render at all on the machine lens (operator finding)").toBe(
    "",
  );
  expect(machineChromePrefixOf(got), "topbar/meta/logscope must still match byte-for-byte; crumb is normalized, see normalizeMachineCrumb's doc").toBe(
    machineChromePrefixOf(golden),
  );
  const gotStage = await machineStageText(page);
  expect(
    gotStage,
    "the machine lens BODY (gauges, residency rows, live load/thermal/power) must match byte-for-byte — see goldenMachineStageText's doc for the #2826 re-widening and its known gaps",
  ).toBe(goldenMachineStageText(golden));
});

test("next: #lens=machine deep-link boot matches goldens/machine-deeplink.txt", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  // A FRESH boot with the hash already set — `useHashRoute`'s initial
  // `getSnapshot()` must resolve `{kind:"machine"}` on the FIRST render,
  // not depend on a `hashchange` event ever firing. Distinct code path from
  // the click-navigation test above (same reasoning as the legacy
  // `#lens=machine` deep-link golden — see `extract.spec.ts`'s comment on
  // that test).
  await page.goto("/index.html#lens=machine");
  await waitSettled(page, expect, '.machine-lens__health[data-state="loaded"]');
  await expect(page.locator("body")).not.toHaveClass(/booting/);

  // (#1809, then narrowed further by #1806 Stage 2/3, then narrowed a THIRD
  // time by the utility-block redesign + its reorder follow-up, then a
  // FOURTH time for `#crumb` itself, then RE-WIDENED #2826) — same split as
  // the click-navigation test above.
  const got = await extractLensText(page);
  const golden = readGolden("machine-deeplink");
  expect(await regionText(page, "crumb"), "#crumb must not render at all on the machine lens (operator finding)").toBe(
    "",
  );
  expect(machineChromePrefixOf(got), "topbar/meta/logscope must still match byte-for-byte; crumb is normalized, see normalizeMachineCrumb's doc").toBe(
    machineChromePrefixOf(golden),
  );
  const gotStage = await machineStageText(page);
  expect(
    gotStage,
    "the machine lens BODY (gauges, residency rows, live load/thermal/power) must match byte-for-byte — see goldenMachineStageText's doc for the #2826 re-widening and its known gaps",
  ).toBe(goldenMachineStageText(golden));
});

// Red-prove — the SAME self-test discipline the legacy harness's
// redprove.spec.ts applies, run against the NEW UI: a blank/unreachable
// daemon must NOT produce text matching either golden. "A probe that passes
// without executing is worse than no probe" (operator doctrine) applies
// here exactly as it does to the legacy side.
test("next: blank daemon fails both machine-lens golden comparisons", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1));
  installBlankRoutes(page);

  await page.goto("/index.html#lens=machine");
  // The blank page's `/machine/resources` 404s, so `resourcesQuery.data.ok`
  // is false and the health region never reaches `data-state="loaded"` —
  // wait on the error/loading terminal state instead (mirrors
  // redprove.spec.ts's `.none`-with-text distinction on the legacy side:
  // CSS state alone can't tell "settled-but-unreachable" from "still
  // loading", so this waits on whichever of the two non-loading states
  // actually appears).
  const settled = page.locator(
    '.machine-lens__health[data-state="loaded"], .machine-lens__health[data-state="error"]',
  );
  await waitSettled(page, expect, settled);

  const got = await extractLensText(page);
  expect(got, "redprove FAILED: a blank/unreachable daemon must not match the real machine golden").not.toBe(readGolden("machine"));
  expect(got, "redprove FAILED: a blank/unreachable daemon must not match the real machine-deeplink golden").not.toBe(
    readGolden("machine-deeplink"),
  );
});

test("next: render-sanity — zero pageerror, no horizontal scroll at 390px, real stage height", async ({ page }) => {
  const pageErrors = [];
  const consoleErrors = [];
  page.on("pageerror", (err) => pageErrors.push(String(err)));
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text());
  });

  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  // Playwright viewport trap (overnight runbook): `devices['Desktop Chrome']`
  // spread in the project's `use` (next-parity.playwright.config.js) would
  // override a config-level `viewport` — `page.setViewportSize()` called
  // HERE, at runtime, always wins regardless, which is why this is the safe
  // way to force 390px rather than fighting the config.
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/index.html#lens=machine");
  await waitSettled(page, expect, '.machine-lens__health[data-state="loaded"]');

  const stageBox = await page.locator("#stage").boundingBox();
  expect(stageBox, "the #stage region must have a real bounding box").not.toBeNull();
  expect(stageBox.height).toBeGreaterThan(20);

  // `document.body.scrollWidth`, NOT `document.documentElement.scrollWidth`
  // — `styles.css` sets `overflow-x: hidden` on both html AND body, which
  // CLAMPS `documentElement.scrollWidth` to the viewport width (see
  // `ui/verify/live-render.spec.ts`'s identical comment — this is the same
  // gotcha, ported verbatim into the parity harness's own render-sanity
  // check since the machine lens is dense enough (30 run rows, memcards) to
  // be a real overflow-risk candidate, unlike the fleet strip's few cards).
  const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
  expect(overflow, "no horizontal document scroll at 390px").toBe(false);

  expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
  expect(consoleErrors, `console.error events: ${consoleErrors.join("; ")}`).toHaveLength(0);

  await page.screenshot({ path: screenshotPath("machine-390px.png"), fullPage: true });
});

test("next: render-sanity screenshot at desktop width (populated, for review)", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/index.html#lens=machine");
  await waitSettled(page, expect, '.machine-lens__health[data-state="loaded"]');
  await page.screenshot({ path: screenshotPath("machine-1280px.png"), fullPage: true });
});

test("next: render-sanity screenshot of the deep-link boot path", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/index.html#lens=machine");
  await waitSettled(page, expect, '.machine-lens__health[data-state="loaded"]');
  await page.screenshot({ path: screenshotPath("machine-deeplink-390px.png"), fullPage: true });
});

// ── Packet 8 — the fleet default view (the savings hero + machine cards +
// activity timeline), superseding the scaffold's original `FleetStrip`
// presence-only proof region. `#lens=fleet` doesn't exist as a hash value
// in the legacy grammar — the fleet lens is what a BARE `/index.html` (no
// hash at all) resolves to (`parseRoute()`'s final fallback) — so unlike
// the machine lens above there is only ONE boot path to test, not a
// click-navigation/deep-link pair: every fresh load of the app lands here.
const FLEET_GALLERY_DIR = process.env.DARKMUX_GALLERY_DIR || path.join(__dirname, ".gallery", "8-fleet");
function fleetShot(name) {
  return path.join(FLEET_GALLERY_DIR, name);
}
test.beforeAll(() => {
  mkdirSync(FLEET_GALLERY_DIR, { recursive: true });
});

// The fleet lens's own post-fetch content marker (mirrors the machine
// lens's `.machine-lens__health[data-state="loaded"]` above): `FleetLens`
// renders its FULL structure (hero + cards + timeline) immediately, even
// before `useFlowWindow` settles — the hero always-renders-even-at-zero by
// design (see `SavingsHero`'s own doc) — so `.fleet-lens` itself attaches
// to the DOM on first paint, well before real data has arrived. Waiting on
// bare `.fleet-lens` (or even legacy's own `#stage .fleet` marker, which
// this port's `.fleet` cards div ALSO satisfies unconditionally) would race
// the fetch exactly the way `extract-lens.js`'s module doc warns against.
// `data-state="loaded"` is stamped only once BOTH day-fetches
// (`useFlowWindow`'s `settled`) have resolved — the same two-fetch gate the
// numbers/cards/timeline all actually depend on.
const FLEET_LOADED = '.fleet-lens[data-state="loaded"]';

test("next: fresh boot (no hash) into the fleet lens matches goldens/fleet.txt", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.goto("/index.html");
  await waitSettled(page, expect, FLEET_LOADED);
  await expect(page.locator("body")).not.toHaveClass(/booting/);

  const got = await extractLensText(page);
  expect(got).toBe(readGolden("fleet"));
});

// Red-prove — same self-test discipline as the machine lens's own redprove
// test above: a blank/unreachable daemon must NOT produce text matching the
// real golden. `flowWindow.settled` still flips true on a blank harness
// (both day-fetches resolve to a non-ok `FetchResult`, which is still a
// SETTLED query state, not a pending one) — so `FLEET_LOADED` appears here
// too, just fronting all-zero/empty content instead of the real corpus.
test("next: blank daemon fails the fleet-lens golden comparison", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1));
  installBlankRoutes(page);

  await page.goto("/index.html");
  await waitSettled(page, expect, FLEET_LOADED);

  const got = await extractLensText(page);
  expect(got, "redprove FAILED: a blank/unreachable daemon must not match the real fleet golden").not.toBe(readGolden("fleet"));
});

test("next: fleet lens render-sanity — zero pageerror, no horizontal scroll at 390px, real stage height", async ({ page }) => {
  const pageErrors = [];
  const consoleErrors = [];
  page.on("pageerror", (err) => pageErrors.push(String(err)));
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text());
  });

  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/index.html");
  await waitSettled(page, expect, FLEET_LOADED);

  const stageBox = await page.locator("#stage").boundingBox();
  expect(stageBox, "the #stage region must have a real bounding box").not.toBeNull();
  expect(stageBox.height).toBeGreaterThan(20);

  // `document.body.scrollWidth`, NOT `document.documentElement.scrollWidth`
  // — see the machine lens's identical render-sanity test above for why.
  const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
  expect(overflow, "no horizontal document scroll at 390px").toBe(false);

  expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
  expect(consoleErrors, `console.error events: ${consoleErrors.join("; ")}`).toHaveLength(0);

  await page.screenshot({ path: fleetShot("fleet-390px.png"), fullPage: true });
});

test("next: fleet lens render-sanity screenshot at desktop width (populated, for review)", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, meta.frozen_clock_ms);
  installCorpusRoutes(page, meta);

  await page.setViewportSize({ width: 1280, height: 900 });
  await page.goto("/index.html");
  await waitSettled(page, expect, FLEET_LOADED);
  await page.screenshot({ path: fleetShot("fleet-1280px.png"), fullPage: true });
});

// ── #2702 — the CONCATENATED two-day window.
//
// Every live boot of this app fetches TWO days (`useFlowWindow`:
// `[prevDateUTC(today), today]`) and folds them through `buildFlowWindow`,
// which keeps `ts >= nowMs - LIVE_WINDOW_MS`. There is no upper bound in that
// filter — the live playhead is `computeTMax(data)`, not `now` — so how much
// of YESTERDAY survives is a function of the wall clock alone, and "the
// two-day window" is a family of inputs, not one input.
//
// `fleet.txt` above pins exactly one member of that family: at
// `meta.frozen_clock_ms` (2026-08-08T16:40:59Z) the 24h boundary lands at
// 2026-08-07T16:40:59Z, and 28 of this corpus's 1,994 yesterday records
// survive it. (Measured, not assumed — #2702 states the golden "renders
// flow-today.json alone", which is 97% true and worth stating exactly: 963
// records reach the hero there, 935 of them today's.) So the one golden this
// suite had covered the END of the family where yesterday contributes
// almost nothing.
//
// The clock below sits at the OTHER end: at 2026-08-08T02:00:00Z the 24h
// boundary lands at 2026-08-07T02:00:00Z, which is earlier than this
// corpus's earliest yesterday record (02:09:42Z), so `buildFlowWindow`
// truncates nothing and the hero sums the whole concatenation — 2,943
// records, both days. That is the input shape an operator sees whenever
// they look at the fleet early in a UTC day, and nothing graded it.
//
// WHY THIS MATTERS, measured rather than argued. Re-key `runKey` (savings.ts)
// to the bare `session_id` — the shape of the experimental change #2702 was
// filed from — and this corpus answers:
//
//   one-day window (fleet.txt's clock) :    0 of   673 playhead positions move
//   two-day window (this test's clock) : 1998 of 2,073 playhead positions move
//                                        LOCAL 600,113 -> 497,992
//                                        CLOUD 396,926 -> 499,047
//                                        (102,121 tokens off the operator's
//                                         own hardware, DISPATCHES 52 -> 42)
//
// A quarter of the hero's headline can move with `fleet.txt` byte-identical.
// That is the gap this golden closes, and the non-vacuity test below is what
// keeps it closed rather than assumed.
//
// ONE artifact of replaying a STATIC corpus at this clock, named so it is not
// read as a port bug: this corpus's today-fixture runs to 14:28Z, which is
// AFTER the 02:00Z clock, and a live daemon could not have handed the viewer
// records from its own future. The only visible consequence is in `=== meta
// ===`: `readyParts` (`ui/src/lib/metaLine.ts`) refuses a negative age
// (`known = nowMs - last >= 0`), so the "· last dispatch Xh ago" suffix
// `fleet.txt` carries is absent here. That guard is real code doing the right
// thing with an impossible input, and this golden pins it.
const TWO_DAY_CLOCK_MS = Date.UTC(2026, 7, 8, 2, 0, 0);

// (#2879, playback parity) A live page judges "now" by the wall clock, here
// the frozen 02:00Z, not by the newest record. The golden used to anchor the
// 24h window and the hero's "last 24h" totals at the corpus's last record
// (14:28 on the 8th, twelve hours AFTER this clock), which no live viewer at
// 02:00 could have seen. The window now ends at 02:00 and records after it
// no longer count.
test("next: fresh boot into the fleet lens over the CONCATENATED two-day window matches goldens/fleet-two-day.txt", async ({ page }) => {
  const meta = loadMeta();
  await installFrozenClock(page, TWO_DAY_CLOCK_MS);
  installCorpusRoutes(page, meta);

  await page.goto("/index.html");
  await waitSettled(page, expect, FLEET_LOADED);
  await expect(page.locator("body")).not.toHaveClass(/booting/);

  const got = await extractLensText(page);
  expect(got).toBe(readGolden("fleet-two-day"));
});

// Red-prove, same discipline as every other golden in this file: a blank
// daemon must not produce text matching the real two-day golden.
test("next: blank daemon fails the two-day fleet golden comparison", async ({ page }) => {
  await installFrozenClock(page, Date.UTC(2026, 0, 1));
  installBlankRoutes(page);

  await page.goto("/index.html");
  await waitSettled(page, expect, FLEET_LOADED);

  const got = await extractLensText(page);
  expect(got, "redprove FAILED: a blank/unreachable daemon must not match the real two-day golden").not.toBe(readGolden("fleet-two-day"));
});

/**
 * NON-VACUITY (#2702's actual ask: "a golden that cannot fail is worse than
 * none, and the existing one-day golden is currently in that position for
 * anything cross-day").
 *
 * The two assertions below are ONE claim in two halves, and the second half
 * is the load-bearing one: the SAME mutation, served to the SAME app,
 * *moves* the two-day golden and leaves `fleet.txt` BYTE-IDENTICAL.
 *
 * The mutation adds tokens to YESTERDAY's token-bearing `dispatch complete`
 * records, which raises the two-day window's GENERATED and ALL TOKENS
 * figures. Every one of those records is outside `fleet.txt`'s own 24h
 * boundary (2026-08-07T16:40:59Z), so the one-day golden structurally cannot
 * observe the change — which is the property being demonstrated, not an
 * accident of this fixture.
 *
 * This mutation used to stamp an `endpoint` onto those completions instead,
 * reclassifying them from local to hosted. That stopped moving any rendered
 * text when #2834's stop-gap consolidated the hero's local/cloud/unattributed
 * tiles into one `ALL TOKENS` figure: `savings.ts` still computes the split,
 * but the sum it feeds is invariant under reclassification, so the mutation
 * no longer bit the render. The claim under test is about the WINDOW, not
 * about attribution, so the mechanism moved to one the lens still shows.
 *
 * WHAT THIS DOES *NOT* CLAIM, measured and stated because the next reader
 * will assume otherwise from #2702's wording. On the CURRENT lens there is
 * no cross-day *interaction* left to catch: `runKey` is
 * `(session_id, mission_id)` since #2701/#2709, and ZERO run keys in this
 * corpus span both days (9 session IDs do — `task-review-*-task`,
 * `task-list`, `task-__panel_args__`, `task-view` — but each day's records
 * carry a different `mission_id`, so they land under different keys).
 * Yesterday evidence therefore cannot reach today's tokens today. What the
 * two-day window still catches, and the one-day window still cannot, is any
 * change that MERGES those keys back together — which is exactly what the
 * bare-`session_id` measurement above is. So this test proves reach ("this
 * golden sees evidence `fleet.txt` cannot"), and the golden itself is what
 * catches the merge.
 */
test("next: the two-day golden is non-vacuous where fleet.txt is structurally blind", async ({ page }) => {
  const meta = loadMeta();
  const yesterday = JSON.parse(readFileSync(path.join(CORPUS_DIR, "flow-yesterday.json"), "utf8"));
  let stamped = 0;
  for (const r of yesterday) {
    // BOTH spellings — `darkmux-crew` emits the spaced form, the runtime the
    // dotted one (`crates/darkmux-flow/src/schema.rs`'s own doc), and a
    // mutation that matched only one would quietly stamp nothing.
    if (r?.action !== "dispatch complete" && r?.action !== "dispatch.complete") continue;
    if (!r.payload || typeof r.payload !== "object") continue;
    if (typeof r.payload.completion_tokens !== "number") continue;
    // ONLY records outside `fleet.txt`'s own 24h boundary. That window is
    // ROLLING, not calendar-day, so it already reaches back into yesterday's
    // afternoon — mutating a record inside it moves BOTH goldens and
    // collapses the second assertion, which is the load-bearing half.
    if (Date.parse(r.ts) >= meta.frozen_clock_ms - 24 * 60 * 60 * 1000) continue;
    r.payload.completion_tokens += 100_000;
    if (typeof r.payload.total_tokens === "number") r.payload.total_tokens += 100_000;
    stamped++;
  }
  // The mutation has to BITE, or both assertions below pass vacuously —
  // which is the exact failure this whole test exists to rule out.
  expect(stamped, "the mutation must add tokens to at least one yesterday completion").toBeGreaterThan(0);

  const installMutated = async () => {
    installCorpusRoutes(page, meta);
    // Registered AFTER `installCorpusRoutes`, so it wins: Playwright matches
    // the most recently registered route first (the same ordering
    // `nav-chrome.spec.ts` relies on for its own `/fleet/machines/live`
    // override).
    await page.route(`**/flow/${meta.captured_prev_date}`, (route) =>
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(yesterday) }),
    );
  };

  await installFrozenClock(page, TWO_DAY_CLOCK_MS);
  await installMutated();
  await page.goto("/index.html");
  await waitSettled(page, expect, FLEET_LOADED);
  const twoDay = await extractLensText(page);
  expect(twoDay, "NON-VACUITY FAILED: the two-day golden did not move on cross-day evidence that changes a session's tokens").not.toBe(readGolden("fleet-two-day"));

  await installFrozenClock(page, meta.frozen_clock_ms);
  await page.goto("/index.html");
  await waitSettled(page, expect, FLEET_LOADED);
  const oneDay = await extractLensText(page);
  expect(oneDay, "fleet.txt must be blind to this — that blindness is the gap #2702 names").toBe(readGolden("fleet"));
});
