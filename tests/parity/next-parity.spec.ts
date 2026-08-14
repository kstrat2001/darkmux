// @ts-nocheck
// The NEW-UI ACCEPTANCE GATE (Packet 2 — the machine lens, the first real
// lens ported onto the React + TanStack Query scaffold). Loads the
// COMMITTED BUILT `/next` artifact (`crates/darkmux-serve/assets/next.html`,
// served via `next-parity.playwright.config.js`'s dedicated webServer) the
// same way `extract.spec.ts` loads the legacy `viewer.html`: the SAME
// sanitized corpus fixtures via `page.route()` interception
// (`installCorpusRoutes`, `lib/mock-routes.js` — shared verbatim with the
// legacy extraction, not a lookalike reimplementation), the SAME frozen
// clock, the SAME extraction logic (`lib/extract-lens.js`'s
// `extractLensText`, also shared verbatim). Then it asserts the result
// against `goldens/machine.txt` / `goldens/machine-deeplink.txt` — the
// executable spec `extract.spec.ts` recorded from the legacy viewer.
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
import { GOLDENS_DIR } from "./lib/paths.js";
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
 * (`0 · 34 · 69 · 103 · 137 · LIMIT`), a tell-tale lamp row that renders
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
 * What survives THIS byte comparison, narrower still than the prior cut:
 * ONLY the stage's header line (`.machine-lens__hdr` — "fleet › machine ·
 * <label> — <spec>") — see `goldenMachineHdrText`/`machineHdrText` below.
 * That line is real chrome untouched by any of it (it renders before
 * `.machine-lens__health` and always has), so it stays a meaningful
 * byte-exact tie. If it starts failing, that IS real coverage: the page
 * header broke, not a rubber stamp.
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
  return fullText.slice(0, idx);
}

/** The golden's own stage text, sliced down to JUST the header line — the
 * one region of the OLD golden that still corresponds to something real in
 * the new markup (see this section's own doc for the full history of what
 * else used to be covered here and why each piece was retired). */
function goldenMachineHdrText(goldenText: string): string {
  const stageMarker = "=== stage ===\n";
  const stageIdx = goldenText.indexOf(stageMarker);
  if (stageIdx === -1) throw new Error(`goldenMachineHdrText: no "${stageMarker.trim()}" marker found`);
  const stage = goldenText.slice(stageIdx + stageMarker.length);
  // Just the header — "machine total" (Stage 2/3's ledger) and
  // "darkmux/utility" (the utility-block redesign) both begin retired
  // portions now.
  const lines = stage.split("\n").slice(0, 1);
  return normalize(lines.join("\n"));
}

async function machineHdrText(page): Promise<string> {
  const got: string = await page.evaluate(() => {
    const el = document.querySelector(".machine-lens__hdr") as HTMLElement | null;
    return el ? el.innerText : "";
  });
  return normalize(got);
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
  // time by the utility-block redesign + its reorder follow-up) See this
  // file's own `machineChromePrefixOf`/`goldenMachineHdrText` doc for
  // exactly which regions no longer correspond (the runs list, then the
  // whole flat ledger, then the utility block itself) and why each is a
  // deliberate divergence, not a regression.
  const got = await extractLensText(page);
  const golden = readGolden("machine");
  expect(machineChromePrefixOf(got), "topbar/crumb/meta/logscope must still match byte-for-byte").toBe(
    machineChromePrefixOf(golden),
  );
  const gotHdr = await machineHdrText(page);
  expect(
    gotHdr,
    "the stage's header line must still match byte-for-byte — everything below it (the ledger, then the utility block, which is now deleted outright) was deliberately redesigned, see this file's own doc for the replacement coverage",
  ).toBe(goldenMachineHdrText(golden));
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
  // time by the utility-block redesign + its reorder follow-up) — same
  // split as the click-navigation test above.
  const got = await extractLensText(page);
  const golden = readGolden("machine-deeplink");
  expect(machineChromePrefixOf(got), "topbar/crumb/meta/logscope must still match byte-for-byte").toBe(
    machineChromePrefixOf(golden),
  );
  const gotHdr = await machineHdrText(page);
  expect(
    gotHdr,
    "the stage's header line must still match byte-for-byte — everything below it (the ledger, then the utility block, which is now deleted outright) was deliberately redesigned, see this file's own doc for the replacement coverage",
  ).toBe(goldenMachineHdrText(golden));
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
