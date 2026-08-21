// @ts-nocheck
// Packet 6 acceptance: `/next`'s ported console (CLI-panel) lens vs the
// legacy goldens grown in this same packet (`goldens/console*.txt` — the
// default `console.txt` predates this packet from 0a; seven more were new
// here; `console-mission-status.txt` is an eighth, added by #1904 once the
// bare default stopped being that CLI panel's own content — see
// `AUTO_PANELS`'s own comment below). Run with `bunx playwright test
// --config next-playwright.config.js` (see `package.json`'s `next-parity`
// script) — NOT picked up by `playwright.config.js`'s `testMatch`, which is
// scoped to the legacy extractor/red-prove pair on purpose.
//
// Same interception + frozen-clock pattern as `extract.spec.ts`, reused
// verbatim via `lib/extract-next-lens.js` — see that module's doc for the
// `#stage`-only scoping rationale. Following `next-parity-runs.spec.ts`'s own
// precedent (its header comment): this file is the only console-specific
// thing here; folding it into a shared `next-parity.spec.ts` (if one lands
// from a parallel packet by merge time) is a rename, not a rewrite.
//
// Settle-marker note (same fix `extract.spec.ts` needed, after `bun run
// determinism` caught a real race there): `ConsolePanel.tsx`'s
// `LoadedChrome`/`NotLoadedChrome` BOTH render a `.pc-cmd` span (a faithful
// port of legacy's own `renderConsole()`, which does the same in both chrome
// branches — see that file's own comment) with often-IDENTICAL text for
// these panel ids, so `.pc-cmd` alone never actually discriminates
// loaded-vs-not. The genuinely exclusive marker is the button's OWN text:
// only `LoadedChrome` renders "re-run" (`NotLoadedChrome` always says "run",
// loading or not) — waiting on that, not the shared `.panelout`/`.pc-cmd`
// selectors, is what makes these assertions non-racy against a near-instant
// mocked fetch. The error-path assertions (red-prove) use `.panelerr`
// instead, since that IS the final state there (see `mock-routes.js`'s
// `installBlankRoutes` — every `/panel/*` path 404s unconditionally, so
// there is no "loaded" branch to race against).

import { test, expect } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { GOLDENS_DIR } from "./lib/paths.js";
import {
  extractStageOnlyText,
  stageSectionOf,
  installFrozenClock,
  installCorpusRoutes,
  installBlankRoutes,
  loadMeta,
} from "./lib/extract-next-lens.js";

// Gitignored, repo-relative by default (`tests/parity/.gallery/6-console/`)
// — see `next-parity-runs.spec.ts`'s identical comment for why this must
// never be an operator absolute path baked into committed source (the
// packet-2/packet-3 QA must-fix this convention exists to prevent).
const GALLERY_DIR = process.env.DARKMUX_GALLERY_DIR || path.join(__dirname, ".gallery", "6-console");
function shot(name: string) {
  return path.join(GALLERY_DIR, name);
}

function goldenStageText(label: string): string {
  const full = readFileSync(`${GOLDENS_DIR}/${label}.txt`, "utf8");
  return stageSectionOf(full);
}

// `re-run` button text is exclusive to the loaded chrome branch (see the
// module doc above); `.panelerr` is the error branch's own class.
const LOADED = '.panelchrome button:text-is("re-run")';
const ERRORED = "#stage .panelerr";
// (#1904) The console's DEFAULT landing state — `panelId === ""` — is no
// longer a CLI panel at all, so neither `LOADED` nor `ERRORED` above ever
// appears there (`ActivityPanel` has no `.panelchrome`/re-run button and no
// `.panelerr` class of its own; a failed `/runs` fetch degrades to the
// same honest-empty render as a genuinely quiet daemon — see that
// component's own module doc for why that's an accepted, precedented
// simplification, the same one `RunsBoard.tsx`'s own doc names for its
// identical `/runs` fetch). `.consoleactivity` is this view's own mount
// marker, corpus-reliable because the recorded corpus has real (if all
// non-running) `/runs` history to render as rows.
const ACTIVITY = ".consoleactivity";

/**
 * Wait for `locatorStr` to attach, PUMPING the frozen page clock forward in
 * small increments while polling. Empirically necessary for any
 * same-document transition (a click, or a `page.goto` to a new hash on an
 * already-loaded page) that triggers a NEW TanStack Query fetch: a single
 * fixed `page.clock.runFor(1000)` right after the click resolved reliably in
 * isolation but flaked under real parallel-worker load (the fixed amount was
 * a magic number, not a bound) — this polls instead, bounded by REAL wall
 * time (`toPass`'s own timeout runs on the TEST RUNNER's clock, never the
 * page's frozen one, so it isn't itself gated by the freeze).
 *
 * A FRESH page load's own initial fetch does NOT need this (see every
 * `page.goto` immediately followed by a bare `toBeAttached()` above) — only
 * a fetch triggered by an already-mounted page does. Root cause not fully
 * chased (see the packet report): something in TanStack Query's own
 * notify/GC scheduling appears to depend on a timer `page.clock`'s pause
 * blocks for an in-page transition but not an initial mount. Ledgered as a
 * FOLLOW-UP worth hoisting into a shared harness helper if a later lens
 * packet hits the same thing.
 */
async function waitLoadedUnderFrozenClock(page, locatorStr, timeoutMs = 15000) {
  await expect(async () => {
    await page.clock.runFor(250);
    await expect(page.locator(locatorStr)).toBeAttached({ timeout: 200 });
  }).toPass({ timeout: timeoutMs, intervals: [50] });
}

// (#1904) `mission-status` joins this list — it was previously covered ONLY
// via the bare default landing (`console.txt`, before this packet WAS its
// content), never through its own explicit deep link, because the default
// and this panel used to be the same thing. Now that the default is the
// activity view, that coverage would silently vanish unless this panel
// gets the same explicit-deep-link test every other CLI panel already has
// — `console-mission-status.txt` is its own golden, a copy of the pre-
// packet `console.txt` body with the same two new tab-bar lines every
// other golden in this file gets.
const AUTO_PANELS = ["mission-status", "mission-status-all", "machine-status", "flow-status", "role-list", "config-list", "lab-fixture-list"];

test.describe("next-parity: console lens (Packet 6)", () => {
  test.beforeAll(() => {
    mkdirSync(GALLERY_DIR, { recursive: true });
  });

  test("default #lens=console boot (no panel param) matches console.txt's #stage", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html#lens=console");
    await expect(page.locator(ACTIVITY)).toBeAttached({ timeout: 15000 });

    const got = await extractStageOnlyText(page);
    expect(got, "console default panel (the activity view) must match the frozen golden byte-for-byte").toBe(goldenStageText("console"));
    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);

    await page.screenshot({ path: shot("console-default.png"), fullPage: true });
  });

  test("an unrecognized panel= id deep-links to the default panel, not a blank page", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);

    await page.goto("/index.html#lens=console&panel=rm-rf-everything");
    await expect(page.locator(ACTIVITY)).toBeAttached({ timeout: 15000 });
    expect(await extractStageOnlyText(page)).toBe(goldenStageText("console"));
  });

  for (const panelId of AUTO_PANELS) {
    test(`#lens=console&panel=${panelId} deep-link boot matches console-${panelId}.txt's #stage`, async ({ page }) => {
      const meta = loadMeta();
      await installFrozenClock(page, meta.frozen_clock_ms);
      installCorpusRoutes(page, meta);

      await page.goto(`/index.html#lens=console&panel=${panelId}`);
      await expect(page.locator(LOADED)).toBeAttached({ timeout: 15000 });
      expect(await extractStageOnlyText(page)).toBe(goldenStageText(`console-${panelId}`));
      await page.screenshot({ path: shot(`console-${panelId}.png`), fullPage: true });
    });
  }

  test("#lens=console&panel=doctor deep-link boot shows the not-yet-run placeholder (manual-only, never auto-fetched)", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const requests: string[] = [];
    page.on("request", (r) => {
      if (new URL(r.url()).pathname.startsWith("/panel/")) requests.push(r.url());
    });

    await page.goto("/index.html#lens=console&panel=doctor");
    await expect(page.getByText(/not run yet — this panel probes the machine/)).toBeAttached({ timeout: 15000 });
    expect(await extractStageOnlyText(page)).toBe(goldenStageText("console-doctor-not-run"));
    expect(requests, "selecting the doctor tab must never auto-fetch (#1286 — the observer must not join the observed)").toHaveLength(0);
    await page.screenshot({ path: shot("console-doctor-not-run.png"), fullPage: true });
  });

  test("clicking doctor's run button fetches it and matches console-doctor.txt's #stage", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);

    await page.goto("/index.html#lens=console&panel=doctor");
    await expect(page.getByText(/not run yet — this panel probes the machine/)).toBeAttached({ timeout: 15000 });
    await page.click('[data-act="refreshpanel"]');
    await waitLoadedUnderFrozenClock(page, LOADED);
    expect(await extractStageOnlyText(page)).toBe(goldenStageText("console-doctor"));
    await page.screenshot({ path: shot("console-doctor.png"), fullPage: true });
  });

  test("clicking through tabs (not just deep-linking) reaches the same content — activity default, role-list, then explicit mission-status", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);

    await page.goto("/index.html#lens=console");
    await expect(page.locator(ACTIVITY)).toBeAttached({ timeout: 15000 });

    await page.click('[data-act="setpanel"][data-arg="role-list"]');
    await expect(page.locator('[data-act="setpanel"][data-arg="role-list"].on')).toBeAttached({ timeout: 15000 });
    await waitLoadedUnderFrozenClock(page, LOADED);
    await expect(page.locator(".panelchrome .pc-cmd")).toContainText("role list");
    expect(await extractStageOnlyText(page)).toBe(goldenStageText("console-role-list"));

    // (#1904) Explicitly picking "mission status" now reaches the CLI
    // panel's own content — a DIFFERENT golden than the bare default
    // landing produces, since the two are no longer the same view. See
    // `AUTO_PANELS`'s own comment for why `console-mission-status.txt`
    // exists as its own file.
    await page.click('[data-act="setpanel"][data-arg="mission-status"]');
    await expect(page.locator('[data-act="setpanel"][data-arg="mission-status"].on')).toBeAttached({ timeout: 15000 });
    await waitLoadedUnderFrozenClock(page, LOADED);
    expect(await extractStageOnlyText(page)).toBe(goldenStageText("console-mission-status"));
  });

  test("the REAL in-corpus OSC-8 deep link ('→ show every mission') renders as a real anchor, since this harness's origin never matches the recorded one", async ({ page }) => {
    // `mission status`'s own recorded output bakes a REAL OSC-8 hyperlink to
    // `#lens=console&panel=mission-status-all` at the OPERATOR'S OWN daemon
    // origin (`http://100.141.193.1:8765/...` post-sanitization —
    // `panel_deep_link` in `src/mission_status.rs`). Tried, as a first pass,
    // to click this real link and assert an in-page panel switch — that
    // FAILED, and reading `ansi.tsx`'s `panelHref`/`panelSwitchId` shows why
    // it MUST fail under this specific harness: the switch only fires for a
    // same-origin (or loopback) href, and this test server serves `/next` on
    // `127.0.0.1:<random port>` (`next-playwright.config.js`), never the
    // operator's real daemon origin the corpus was recorded from — so
    // `panelHref` correctly leaves it ABSOLUTE and FOREIGN, exactly as it
    // would for a genuinely different site, and `panelSwitchId` correctly
    // returns null. This is a harness-shape limitation, not a component bug:
    // in real production use the daemon serves `/next` from the SAME origin
    // its own CLI bakes into `panel_deep_link`, so the same-origin branch
    // fires for real. The MECHANISM itself (same-origin/loopback → in-page
    // switch, foreign origin → real link) is fully unit-tested in
    // `ansi.test.tsx` against a same-origin URL built from
    // `window.location.origin`, which is the correct way to test a pure
    // function whose branch depends on the CALLER's origin. This test
    // instead pins the OTHER branch: the innerText/attribute shape for a
    // link this harness can never make same-origin, so a future refactor
    // that broke the foreign-origin fallback would fail HERE.
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);

    // (#1904) `mission status`'s own OSC-8 content only renders once that
    // CLI panel is actually selected — the bare default no longer lands
    // there (see `ACTIVITY`'s own doc above).
    await page.goto("/index.html#lens=console&panel=mission-status");
    await expect(page.locator(LOADED)).toBeAttached({ timeout: 15000 });

    const deepLink = page.locator('a.a-link', { hasText: "show every mission" });
    await expect(deepLink).toBeAttached({ timeout: 15000 });
    expect(await deepLink.getAttribute("data-act")).toBeNull();
    expect(await deepLink.getAttribute("href")).toContain("panel=mission-status-all");
  });

  test("render-sanity: zero pageerror, real #stage height, no 390px overflow", async ({ page }) => {
    const meta = loadMeta();
    await installFrozenClock(page, meta.frozen_clock_ms);
    installCorpusRoutes(page, meta);
    const pageErrors: string[] = [];
    const consoleErrors: string[] = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));
    page.on("console", (msg) => {
      if (msg.type() === "error") consoleErrors.push(msg.text());
    });

    // Set explicitly AFTER navigation (see `next-parity-runs.spec.ts`'s
    // identical comment) — the device preset alone does not guarantee an
    // actually-390px viewport.
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/index.html#lens=console&panel=mission-status-all");
    await expect(page.locator(LOADED)).toBeAttached({ timeout: 15000 });

    const stageBox = await page.locator("#stage").boundingBox();
    expect(stageBox, "#stage must have a real bounding box").not.toBeNull();
    expect(stageBox!.height).toBeGreaterThan(20);

    // `document.body.scrollWidth`, NOT `documentElement.scrollWidth` — see
    // `next-parity-runs.spec.ts`'s identical comment for the
    // `overflow-x:hidden`-clamp gotcha this sidesteps. `mission-status-all`
    // is deliberately the widest real panel content in the corpus (93
    // missions' worth of table rows) — the panel most likely to actually
    // overflow if `.panelout`'s `overflow-x:auto` scoping were wrong.
    const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
    expect(overflow, "no horizontal document scroll at 390px").toBe(false);

    expect(pageErrors, `pageerror events: ${pageErrors.join("; ")}`).toHaveLength(0);
    expect(consoleErrors, `console.error events: ${consoleErrors.join("; ")}`).toHaveLength(0);

    const width = page.viewportSize()?.width;
    expect(width, "viewport must actually be 390px wide, not overridden by a device preset").toBe(390);

    await page.screenshot({ path: shot("console-390px.png"), fullPage: true });
  });
});

test.describe("next-parity: console lens red-prove (harness self-test)", () => {
  test("blank routes fail every console golden comparison", async ({ page }) => {
    await installFrozenClock(page, Date.UTC(2026, 0, 1));
    installBlankRoutes(page);

    // (#1904) The bare default is `ActivityPanel` now, not a CLI panel —
    // it has no `.panelerr` class to assert against (a failed `/runs` read
    // degrades to the SAME honest-empty render as a quiet daemon; see
    // `ACTIVITY`'s own doc above for why that's accepted here, matching
    // `RunsBoard.tsx`'s own precedent for the same endpoint). What's still
    // genuinely provable on the default itself: a blank daemon does NOT
    // silently reproduce the real `console.txt` golden.
    await page.goto("/index.html#lens=console");
    await expect(page.locator(ACTIVITY)).toBeAttached({ timeout: 15000 });
    expect(await extractStageOnlyText(page), "blank-route extraction must NOT match the real console.txt golden").not.toBe(goldenStageText("console"));

    // A genuinely CLI-backed panel is where `.panelerr` visibility is still
    // a real, checkable behavior — `mission-status`, explicitly selected.
    await page.goto("/index.html#lens=console&panel=mission-status");
    await waitLoadedUnderFrozenClock(page, ERRORED);
    expect(await extractStageOnlyText(page)).not.toBe(goldenStageText("console-mission-status"));

    // Every navigation AFTER this point is a same-document hash change
    // (`page.goto` to a new `#...` on the already-loaded page, or a click) —
    // NOT a fresh page load. Empirically, a same-document transition's
    // TanStack Query re-render never commits while `page.clock` is PAUSED
    // (the very first, fresh-page-load boot above is unaffected — only
    // subsequent in-page updates are); `page.clock.runFor(1000)` pumps the
    // frozen clock forward just enough to unstick whatever internal
    // timer-gated notification TanStack Query is waiting on, without giving
    // up the determinism a frozen clock provides elsewhere. See the
    // "clicking through tabs" test above, and its own comment, for where
    // this was first isolated.
    for (const panelId of AUTO_PANELS) {
      await page.goto(`/index.html#lens=console&panel=${panelId}`);
      await waitLoadedUnderFrozenClock(page, ERRORED);
      expect(await extractStageOnlyText(page)).not.toBe(goldenStageText(`console-${panelId}`));
    }

    // doctor: the not-yet-run placeholder is daemon-INDEPENDENT `#stage`
    // content (no fetch happens at all on tab-select — see
    // `console-doctor-not-run`'s own extract.spec.ts comment for the same
    // point on the legacy side), so `#stage` alone is expected to be
    // IDENTICAL between a real daemon and a blank one for this one golden.
    // `next-parity`'s own goldens are scoped to `#stage` only (see this
    // file's own header + `lib/extract-next-lens.js`'s module doc for why —
    // `#meta`/`#crumb` are Packet-1-scaffold chrome this harness doesn't
    // compare), so there is no OTHER region left to prove non-vacuousness
    // through the way the legacy four-region `redprove.spec.ts` does. What
    // IS genuinely provable here: clicking "run" against a blank daemon
    // lands on the error branch, never the real `console-doctor.txt`
    // content — asserted below.
    await page.goto("/index.html#lens=console&panel=doctor");
    await expect(page.getByText(/not run yet — this panel probes the machine/)).toBeAttached({ timeout: 15000 });
    await page.click('[data-act="refreshpanel"]');
    await waitLoadedUnderFrozenClock(page, ERRORED);
    expect(await extractStageOnlyText(page)).not.toBe(goldenStageText("console-doctor"));
  });
});
