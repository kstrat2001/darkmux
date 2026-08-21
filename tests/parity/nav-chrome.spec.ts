// @ts-nocheck
// Packet 1.5 acceptance: NAV CHROME + HASH WRITE-BACK — the scaffold gap
// both the machine (Packet 2) and runs (Packet 3) lens packets independently
// flagged as a hard blocker for the eventual /next -> / flip. This is chrome,
// not lens CONTENT, so `next-parity.spec.ts`/`next-parity-runs.spec.ts`
// (which grade `#crumb`/`#meta`/`#logscope`/`#stage` — see
// `nav-chrome.playwright.config.js`'s own doc) cannot cover it; this file is
// the dedicated gate.
//
// Same corpus-interception machinery as the other next-parity specs, reused
// verbatim (`lib/mock-routes.js`'s `installCorpusRoutes`, `lib/extract-lens.js`'s
// `waitSettled`) so the machine/runs lenses this suite navigates INTO render
// real content, not an empty/pending skeleton — a click that lands on a
// skeleton would not prove the tab actually selected the right lens.
//
// Deliberately DOES NOT install the frozen clock the byte-parity specs use
// (`installFrozenClock`, from the same module). This spec asserts hash/DOM
// mechanics, never relative-time TEXT, so it doesn't need the determinism a
// frozen clock buys — and installing it actively BREAKS this spec.
//
// THE HARNESS RULE (sharpened per QA, 2026-08-09 — durable, not packet-local):
// `installFrozenClock` does `page.clock.install({time}); page.clock.pauseAt(time)`
// — that second call is load-bearing: `install()` alone sets an origin but
// lets timers keep firing in real wall-clock time from there (the SAME gotcha
// `lib/extract-lens.js`'s own doc names for the extraction side); `pauseAt()`
// is what makes the clock FULLY PAUSED — no `setTimeout`/`requestAnimationFrame`
// fires at all until something explicitly advances it. The failure mode is
// precisely: a REAL `page.click()` OBSERVED THROUGH a TanStack Query carrying
// a `refetchInterval` (`MachineLens`'s `resourcesQuery`,
// `refetchInterval: MACHINE_MEM_POLL_MS`) — under a fully paused clock, the
// fetch's own 200 response lands in cache but the component never re-renders
// past `data-state="loading"`; the same navigation via
// `page.evaluate(() => location.hash = ...)` (what `next-parity.spec.ts`
// uses) is unaffected, and a real click against LOCAL REACT STATE (no query,
// no `refetchInterval`) is ALSO unaffected — proof: `next-parity-runs.spec.ts`'s
// own "kind=lab + ◧ series toggle" test does a genuine `page.click()` on the
// series toggle UNDER a frozen clock and passes cleanly every run, because
// that toggle is `useState`, not a polled query. The bug needs BOTH a real
// click AND a refetchInterval-bearing query sitting downstream of it.
//
// Root-caused by hand (frozen clock temporarily removed, re-added, a
// `debug-chrome.spec.ts` scratch repro file used then deleted). Three-line
// repro, for the next person who hits this shape:
//   await page.clock.install({ time }); await page.clock.pauseAt(time);
//   await page.goto("/index.html");
//   await page.click('[data-act="machine"]'); // stuck at data-state="loading" forever
//
// A harness quirk, not a product bug — `next-parity.spec.ts`'s own
// machine-lens goldens (which DO use the frozen clock, via `location.hash =`
// assignment, never a real click) are unaffected and remain the byte-parity
// source of truth for that lens's rendered content. This spec simply drops
// the frozen clock rather than working around the interaction, since it has
// no need for frozen relative-time text in the first place.

import { test, expect } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { loadMeta, installCorpusRoutes } from "./lib/mock-routes.js";
import { waitSettled } from "./lib/extract-lens.js";
import { CORPUS_DIR } from "./lib/paths.js";

// Gitignored, repo-relative by default (`tests/parity/.gallery/1.5-chrome/`)
// — NOT an operator machine path (the QA must-fix pattern every earlier
// packet in this arc hit and fixed — see `next-parity.spec.ts`'s identical
// comment for the full rationale). `DARKMUX_GALLERY_DIR` overrides;
// `mkdirSync` lives inside `test.beforeAll` (fires only when this suite
// actually RUNS), never at module scope.
const GALLERY_DIR = process.env.DARKMUX_GALLERY_DIR || path.join(__dirname, ".gallery", "1.5-chrome");

test.beforeAll(() => {
  mkdirSync(GALLERY_DIR, { recursive: true });
});

function shot(name) {
  return path.join(GALLERY_DIR, name);
}

// Per-tab: the `data-act` the tab carries, the canonical hash a fresh click
// produces, and a post-fetch content-settle selector proving the RIGHT lens
// actually rendered (never just "something changed") — same discipline
// `extract-lens.js`'s own module doc requires of every navigation in this
// harness.
// `console`'s settle selector was `[data-state="not-ported"]` (the
// `LensPlaceholder` this tab rendered when this file was authored) — QA
// found it stale post-merge (2026-08-09): Packet 6 landed a REAL
// `ConsolePanel` for `#lens=console` in the meantime, which never carries
// that data-state, so the click-navigation test hung waiting for a marker
// that can no longer appear (a twin-drift instance, same class this file's
// own module doc already names for the frozen-clock interaction). It was
// then `.panelchrome` (`ConsolePanel`'s CLI-panel chrome wrapper) until
// #1904: a fresh, un-clicked `#lens=console` now lands on `ActivityPanel`
// (the client-rendered default over `/runs`, no CLI panel involved at
// all), which never renders `.panelchrome` — so THAT marker went stale the
// same way, for the same reason (a real behavior change outrunning a
// harness assumption). `.consoleactivity` is `ActivityPanel`'s own
// unconditional wrapper (see that component's own doc — it renders on
// every branch, including the genuinely-empty one), and this test's own
// click always lands on the bare default (no `panel=` param), so it's the
// one marker guaranteed to appear here — matching this test's actual
// assertions (hash + `.on` class) without needing the content fully
// loaded, same reasoning as before.
const TABS = [
  { act: "fleet", hash: "", settle: '[data-state]:not([data-state="pending"])' },
  { act: "console", hash: "#lens=console", settle: ".consoleactivity" },
  { act: "runs", hash: "#lens=runs", settle: '[data-state="data"], [data-state="pending"]' },
  { act: "machine", hash: "#lens=machine", settle: '.machine-lens__health[data-state="loaded"]' },
];

test.describe("nav chrome (Packet 1.5)", () => {
  test("tabs render in the legacy DOM order (viewer.html:816: fleet, console, runs, machine) at 390px, no horizontal overflow", async ({ page }) => {
    const meta = loadMeta();
    installCorpusRoutes(page, meta);

    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/index.html");
    await expect(page.locator("body")).not.toHaveClass(/booting/);

    const tabs = page.locator(".app-shell__navtabs .nav-tab");
    await expect(tabs).toHaveCount(4);
    expect(await tabs.allTextContents()).toEqual(["fleet", "console", "runs", "machine"]);
    expect(await tabs.evaluateAll((els) => els.map((e) => e.id))).toEqual(["lens-fleet", "lens-console", "lens-runs", "lens-machine"]);

    // Same overflow probe every other packet's render-sanity test uses —
    // `document.body.scrollWidth`, NOT `documentElement.scrollWidth` (which
    // `styles.css`'s `overflow-x:hidden` clamps to the viewport and would
    // pass silently against a real overflow). See `ui/verify/live-render.spec.ts`.
    const overflow = await page.evaluate(() => document.body.scrollWidth > document.documentElement.clientWidth);
    expect(overflow, "no horizontal document scroll at 390px with the nav chrome present").toBe(false);

    // Tap-target sanity: every tab is at least ~36px tall (the packet's own
    // padding choice — see `styles.css`'s `.nav-tab` comment).
    for (let i = 0; i < 4; i++) {
      const box = await tabs.nth(i).boundingBox();
      expect(box, `tab ${i} must have a real bounding box`).not.toBeNull();
      expect(box.height, `tab ${i} tap target is only ${box.height}px tall`).toBeGreaterThanOrEqual(30);
    }

    await page.screenshot({ path: shot("chrome-390px-fleet.png"), fullPage: true });
  });

  test("clicking each tab updates BOTH the rendered lens and location.hash, in legacy order", async ({ page }) => {
    const meta = loadMeta();
    installCorpusRoutes(page, meta);
    const pageErrors = [];
    page.on("pageerror", (e) => pageErrors.push(String(e)));

    await page.goto("/index.html");
    await expect(page.locator("body")).not.toHaveClass(/booting/);
    await waitSettled(page, expect, TABS[0].settle);

    for (const { act, hash } of TABS) {
      await page.click(`[data-act="${act}"]`);
      await waitSettled(page, expect, TABS.find((t) => t.act === act).settle);
      const gotHash = await page.evaluate(() => location.hash);
      expect(gotHash, `clicking the "${act}" tab must set location.hash to "${hash}"`).toBe(hash);
      // The clicked tab is the one carrying `.on`; every OTHER tab must not.
      const onIds = await page.locator(".nav-tab.on").evaluateAll((els) => els.map((e) => e.id));
      expect(onIds, `exactly one tab (lens-${act}) should be highlighted after clicking it`).toEqual([`lens-${act}`]);
      await page.screenshot({ path: shot(`chrome-tab-${act}.png`), fullPage: true });
    }

    expect(pageErrors, `pageerror events across the whole click sequence: ${pageErrors.join("; ")}`).toHaveLength(0);
  });

  test("the legacy #lens=lab alias boots and the address bar upgrades to the canonical #lens=runs&kind=lab", async ({ page }) => {
    const meta = loadMeta();
    installCorpusRoutes(page, meta);

    await page.goto("/index.html#lens=lab");
    await waitSettled(page, expect, '[data-state="data"], [data-state="pending"]');

    await expect(async () => {
      const gotHash = await page.evaluate(() => location.hash);
      expect(gotHash).toBe("#lens=runs&kind=lab");
    }).toPass({ timeout: 5000 });

    // And the runs tab is the one lit — the upgrade is a REAL navigation
    // outcome, not just an address-bar cosmetic.
    await expect(page.locator("#lens-runs")).toHaveClass(/\bon\b/);
    await page.screenshot({ path: shot("chrome-lens-lab-upgrade.png"), fullPage: true });
  });

  test("a runs-lens kind-chip click writes the hash directly, without disturbing the active tab", async ({ page }) => {
    const meta = loadMeta();
    installCorpusRoutes(page, meta);

    await page.goto("/index.html#lens=runs");
    await waitSettled(page, expect, '[data-state="data"], [data-state="pending"]');
    expect(await page.evaluate(() => location.hash)).toBe("#lens=runs");

    const missionChip = page.locator('[data-arg="mission"]');
    if ((await missionChip.count()) === 0) {
      test.skip(true, "no mission-kind chip present in this corpus state — nothing to click");
    }
    await missionChip.click();

    await expect(async () => {
      expect(await page.evaluate(() => location.hash)).toBe("#lens=runs&kind=mission");
    }).toPass({ timeout: 5000 });
    await expect(page.locator("#lens-runs")).toHaveClass(/\bon\b/);
    await page.screenshot({ path: shot("chrome-runs-kind-chip.png"), fullPage: true });
  });

  // QA regression test (2026-08-09) for the write-back echo MUST-FIX:
  // `writeHash`'s `replaceState` fires ZERO `hashchange` events, so
  // `useHashRoute`'s module-level `cachedHref` goes stale the instant a
  // kind chip is clicked. The NEXT App re-render for ANY unrelated reason
  // (App's own `useLiveMachines()` polls `/fleet/machines/live` every
  // `PRESENCE_POLL_MS` — 5s) then recomputes a fresh `Route` whose
  // `runsKind` matches what the operator already selected, which
  // `RunsBoard`'s `useEffect([initialKind])` used to treat as a BRAND NEW
  // deep-link and reset `series`/`showAll`/the row-click notice out from
  // under the operator. Fixed by a `if (initialKind === kind) return;`
  // guard in that effect (see `RunsBoard.tsx`).
  //
  // QA's own diagnosis of why the OTHER tests in this file (and every
  // corpus-backed next-parity spec) never caught it: the corpus is a
  // STATIC fixture, so every `/fleet/machines/live` poll returns
  // byte-identical content — TanStack Query's structural sharing collapses
  // that into a no-op query-state change, App never actually re-renders,
  // and the echo path is never exercised. This test defeats structural
  // sharing on purpose: `page.route` handlers are LIFO (the LAST
  // registered one wins for a matching URL), so registering a SECOND
  // `/fleet/machines/live` handler AFTER `installCorpusRoutes` overrides
  // the corpus fixture with a response that bumps `beat_ts_ms` on every
  // single call — a genuinely NEW value each poll, which forces a real
  // re-render.
  test("a runs-lens kind-chip selection survives an unrelated App re-render (regression: the write-back echo must not reset the board)", async ({
    page,
  }) => {
    const meta = loadMeta();
    installCorpusRoutes(page, meta);

    const liveMachinesFixture = JSON.parse(readFileSync(path.join(CORPUS_DIR, "fleet-machines-live.json"), "utf8"));
    let pollCount = 0;
    // Registered AFTER installCorpusRoutes -> Playwright routes LIFO -> this
    // one wins. See the module-level comment above for why the bumped
    // `beat_ts_ms` (not just "any handler") is the load-bearing part.
    await page.route("**/fleet/machines/live", (route) => {
      pollCount++;
      // (#1729) The endpoint is an envelope — `{machines, meta}` — so the
      // bumped beats have to be rebuilt INSIDE it. Serving a bare array here
      // would make the app read zero machines and the regression this test
      // guards would go quiet for the wrong reason.
      const fresh = {
        ...liveMachinesFixture,
        machines: (liveMachinesFixture.machines ?? []).map((m) => ({ ...m, beat_ts_ms: Date.now() })),
      };
      route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(fresh) });
    });

    await page.goto("/index.html#lens=runs");
    await waitSettled(page, expect, '[data-state="data"], [data-state="pending"]');

    // Operator action: select kind=lab, then toggle the series view — the
    // exact sequence QA's live-browser repro used.
    await page.click('[data-arg="lab"]');
    await expect(page.locator('[data-arg="lab"].on')).toBeAttached();
    await page.click('[data-arg="series"]');
    await expect(page.locator('[data-arg="series"].on')).toBeAttached();

    const headerBefore = await page.locator(".stagehdr").innerText();
    expect(headerBefore, "the series view's own header must actually say 'series' before we can prove it survives").toMatch(/series/);

    // Touch nothing. Wait long enough for the 5s presence poll to fire at
    // least once (and for React to process whatever re-render it triggers).
    await page.waitForTimeout(8_000);

    expect(pollCount, "the presence-poll override must have actually fired at least once, or this test proves nothing").toBeGreaterThan(0);

    const headerAfter = await page.locator(".stagehdr").innerText();
    expect(
      headerAfter,
      `the runs board must NOT revert out of series view just because an unrelated poll re-rendered the app (got: "${headerAfter}")`,
    ).toMatch(/series/);
  });
});
