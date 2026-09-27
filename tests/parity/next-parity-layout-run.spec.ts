// @ts-nocheck
// The run page's MODEL section and the fleet card keep ONE size in every
// state (operator, 2026-09-26: "no new height, and nothing that appears or
// disappears and shifts layout between states").
//
// Measured before this suite existed (release builds, a scratch
// DARKMUX_HOME, this suite's own fixture): the #2926 readout line under the
// lamps ("tool gen · write · 18s") grew the MODEL section 35px on a phone
// and 42px on a desktop while the model generated a tool call, and every
// lamp going dark (a finished run, no model working between steps) shrank
// the lamp row 7px. The readout now sits out of flow in the hero's bottom
// padding and the lamp row holds a label line's height with every lamp off
// (`.modelbox__status` / `.scope-lamps::before` in `ui/src/styles.css`).
//
// Each state first asserts the words it must show, so a fixture that slid
// into another state fails here rather than measuring one state N times.
const { test, expect } = require("@playwright/test");
const { STATES, PLAYBACK_NOW, VIEWPORTS, installLayoutRoutes, measure } = require("./lib/layout-fixture.js");

const RUN = {
  modelbox: ".session-run .modelbox",
  modelMain: ".session-run .modelbox__main",
  modelHero: ".session-run .modelbox__hero",
  lamps: ".session-run .modelbox__hero .scope-lamps",
  tube: ".session-run .modelbox__hero .token-scope-bezel",
};
// (#2950) The readout line under the lamps. Asserted for its words, never
// measured into a size group: it is present in some states and absent in
// others, and the boxes above must not notice.
const NOTE = ".session-run .modelbox__hero .modelbox__note";
// (#2915) `util`: the utility strip at the end of the name row. It is always
// there, so it too must keep one size whatever the machine's utility job.
const CARD = { card: ".mach", cardScope: ".mach-scope", rateLine: ".mach-scope__rate", util: ".mach-util" };

// (#2950 review, CONSIDER 2) A REST reason must FIT its one-line slot, not
// just be in it: `toHaveText` (inner text included) reads the whole string
// even when `text-overflow: ellipsis` has cut it off on screen.
async function expectFits(locator, what) {
  const { sw, cw } = await locator.evaluate((e) => ({ sw: e.scrollWidth, cw: e.clientWidth }));
  expect(sw, `${what}: the reason is clipped (scrollWidth ${sw} > clientWidth ${cw})`).toBeLessThanOrEqual(cw);
}

async function openState(browser, viewport, state, { mode, surface }) {
  const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
  const page = await ctx.newPage();
  await page.clock.setFixedTime(mode === "live" ? state.nowMs : PLAYBACK_NOW);
  await installLayoutRoutes(page, { blockStream: state.blockStream === true });
  const hash = surface === "run" ? `#dispatch=${state.runSid ?? state.sid}` : mode === "live" ? "#lens=fleet" : `#${state.date}`;
  await page.goto(`/index.html${hash}`);
  return { ctx, page };
}

/** Group states by their measured size; one group means one size. */
function sizeGroups(rows, key) {
  const groups = new Map();
  for (const r of rows) {
    const k = JSON.stringify(r[key]);
    groups.set(k, [...(groups.get(k) ?? []), r.state]);
  }
  return [...groups.entries()].map(([size, states]) => `${size} <- ${states.join(", ")}`);
}

for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
  for (const mode of ["live", "playback"]) {
    test(`run page MODEL section: one size in every state (${vpName}, ${mode})`, async ({ browser }) => {
      const states = STATES.filter((s) => mode === "live" || s.runPlayback !== false);
      const rows = [];
      for (const state of states) {
        const { ctx, page } = await openState(browser, viewport, state, { mode, surface: "run" });
        const lamps = page.locator(RUN.lamps);
        await expect(lamps, `${state.id}: the page must reach this state before it is measured`).toHaveAttribute("aria-label", state.runText instanceof RegExp ? state.runText : new RegExp(state.runText));
        // (#2950) A state that names its readout line shows exactly it, or
        // (`null`) shows none.
        if (state.noteText === null) await expect(page.locator(NOTE), `${state.id}: no readout line`).toHaveCount(0);
        else if (state.noteText) await expect(page.locator(NOTE), `${state.id}: the readout line`).toHaveText(state.noteText);
        if (state.rateTextPhone) await expectFits(page.locator(NOTE), `${state.id}: run page readout (${vpName}, ${mode})`);
        // Settle: the count-ups and the scope's morph run on timers, and a
        // size taken mid-frame would be a flake, not a finding.
        await page.waitForTimeout(400);
        rows.push({ state: state.id, armed: state.armed === true, ...(await measure(page, RUN)) });
        await ctx.close();
      }
      // (#2950) One size per run CONFIG: a run with the thermal governor
      // armed names its thermal rest under ACTIVE TIME from its start, so its
      // states are measured against each other, not against an unarmed run's.
      expect(rows.filter((r) => r.armed).length, "the armed config has states to compare").toBeGreaterThanOrEqual(5);
      for (const armed of [false, true]) {
        const config = rows.filter((r) => r.armed === armed);
        for (const key of Object.keys(RUN)) {
          const groups = sizeGroups(config, key);
          expect(groups, `${key} changed size between states (${vpName}, ${mode}, thermal ${armed ? "armed" : "off"}):\n  ${groups.join("\n  ")}`).toHaveLength(1);
        }
      }
      // The readout under the lamps is present in the tool-gen states and
      // absent elsewhere; that is the case the layout must absorb.
      expect(rows.map((r) => r.state)).toEqual(
        expect.arrayContaining(["toolgen-named", "finished", "compacting", "radio-routing", "rest", "rest-turn-delay", "rest-thermal", "rest-pacing", "rest-battery", "rest-episode-limit", "rest-unknown", "armed-generating", "armed-toolgen"]),
      );
    });

    test(`fleet card: one size across its running states (${vpName}, ${mode})`, async ({ browser }) => {
      const states = STATES.filter((s) => s.fleet !== false);
      const rows = [];
      for (const state of states) {
        const { ctx, page } = await openState(browser, viewport, state, { mode, surface: "fleet" });
        await expect(page.locator(CARD.card).first()).toBeVisible();
        if (state.rateTextPhone) {
          // (#2950) The words the card SHOWS (`innerText` skips the hidden
          // form): with the state on a desktop card, without it on a phone.
          const want = vpName === "phone" ? state.rateTextPhone : state.rateText;
          await expect(page.locator(CARD.rateLine).first(), `${state.id}: the card's visible reason`).toHaveText(want, { useInnerText: true });
          await expectFits(page.locator(CARD.rateLine).first(), `${state.id}: fleet card status line (${vpName}, ${mode})`);
        } else if (state.rateText) {
          await expect(page.locator(CARD.rateLine).first(), `${state.id}: the card must reach this state`).toHaveText(state.rateText instanceof RegExp ? state.rateText : new RegExp(state.rateText));
        } else {
          await expect(page.locator(CARD.rateLine)).toHaveCount(0);
        }
        // (#2915) The utility strip shows the state's job, or is quiet.
        await expect(page.locator(CARD.util).first(), `${state.id}: the utility strip`).toHaveAttribute("data-visual", state.utilVisual ?? "quiet");
        await expect(page.locator(CARD.util).first(), `${state.id}: the strip's stall`).toHaveAttribute("data-stalled", state.utilStalled ? "true" : "false");
        await page.waitForTimeout(400);
        rows.push({ state: state.id, running: !!state.rateText, ...(await measure(page, CARD)) });
        await ctx.close();
      }
      expect(rows.map((r) => r.state)).toEqual(
        expect.arrayContaining(["compacting", "radio-routing", "utility-generic", "utility-stalled", "rest", "rest-thermal", "rest-episode-limit"]),
      );
      const running = rows.filter((r) => r.running);
      for (const key of ["card", "cardScope", "rateLine", "util"]) {
        const groups = sizeGroups(running, key);
        expect(groups, `${key} changed size between running states (${vpName}, ${mode}):\n  ${groups.join("\n  ")}`).toHaveLength(1);
      }
      // On a phone the card is also the same size idle and running.
      // (#2915) The strip is one size in EVERY state, idle included.
      expect(sizeGroups(rows, "util"), `the utility strip changed size (${vpName}, ${mode})`).toHaveLength(1);
      if (vpName === "phone") {
        for (const key of ["card", "cardScope"]) {
          const groups = sizeGroups(rows, key);
          expect(groups, `${key} changed size between states (${vpName}, ${mode}):\n  ${groups.join("\n  ")}`).toHaveLength(1);
        }
      }
    });
  }
}

// (#2958) Before its first data a fleet card says "no signal" (its stat word
// and a static tube), in the same boxes: the same size as the same card once
// loaded idle, and on a phone as running too. Live only: a replay has its
// records in hand and never waits. The loading page is the "finished" state's
// day with `/runs` (online) or presence (offline) never answered.
//
// (#2958 review M1) An OFFLINE card keeps the tube's box, its screen powered
// off, so it too is one size loading and loaded: the "offline" variant adds a
// rostered machine that is never seen, beside the fixture's own.
for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
  for (const variant of ["online", "offline"]) {
    test(`fleet card: the same size before its first data as loaded (${vpName}, ${variant})`, async ({ browser }) => {
      const byId = Object.fromEntries(STATES.map((s) => [s.id, s]));
      const offline = variant === "offline";
      const rows = [];
      for (const [id, state, hold] of [
        ["loading", byId.finished, true],
        ["loaded", byId.finished, false],
        ...(vpName === "phone" ? [["generating", byId.generating, false]] : []),
      ]) {
        const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
        const page = await ctx.newPage();
        await page.clock.setFixedTime(state.nowMs);
        await installLayoutRoutes(page, offline ? { roster: true, holdPresence: hold } : { holdRuns: hold });
        await page.goto("/index.html#lens=fleet");
        const cards = page.locator(CARD.card);
        await expect(cards).toHaveCount(offline ? 2 : 1);
        const want = id === "loading" ? ["no signal"] : id === "loaded" ? ["idle"] : ["dispatch in flight"];
        if (offline) want.push(id === "loading" ? "no signal" : "offline");
        await expect(page.locator(".mach .stat"), `${id}: the cards' stat words`).toHaveText(want);
        if (id === "loading") {
          await expect(page.locator(".mach .runs").first(), "loading: no count yet").toHaveText("—");
          for (let i = 0; i < want.length; i++) {
            await expect(cards.nth(i).locator(".token-scope-bezel"), "loading: the no-signal tube").toHaveAttribute("data-state", "nosignal");
          }
        } else if (offline) {
          await expect(cards.nth(1).locator(".token-scope-bezel"), "offline: the powered-off tube").toHaveAttribute("data-state", "off");
        }
        await page.waitForTimeout(400);
        const m = await measure(page, { card: CARD.card, cardScope: CARD.cardScope, util: CARD.util, stat: ".mach .stat", runs: ".mach .runs" });
        // The stat and count lines hold their words, so only their HEIGHT is
        // the box: "no signal" and "idle" are different widths of one line.
        rows.push({ state: id, ...m, stat: m.stat.map((b) => b.h), runs: m.runs.map((b) => b.h) });
        await ctx.close();
      }
      for (const key of ["card", "cardScope", "util", "stat", "runs"]) {
        const groups = sizeGroups(rows, key);
        expect(groups, `${key} changed size between loading and loaded (${vpName}, ${variant}):\n  ${groups.join("\n  ")}`).toHaveLength(1);
      }
    });
  }
}

// On a desktop the card is 23px taller while a dispatch runs than idle: the
// rate line under the status appears only with a running execution. That is
// on origin/main as of 93709c0c9 (measured: 282px idle, 305px running), not
// introduced by the branch that added this suite, and fixing it is the
// operator's design call (reserve the line on an idle card, or fold the rate
// into an existing line), so it is recorded here, not decided here.
test.fixme("fleet card: the same size idle and running on a desktop (owner: the operator's call on where the rate line lives)", async () => {});

// (#2950, found while adding the thermal REST states) With the thermal
// governor armed, a FINISHED run's MODEL section is shorter than the same run
// live: ACTIVE TIME's sub line reads "so far · 0 s thermal rest" while live
// and wraps, and "0 s thermal rest" once finished and does not. Measured on
// this fixture with an armed finished run: 414px live vs 386.6px finished on
// a 1280px desktop, 905.1px vs 842.8px on a phone. On origin/main since #2890
// (the sub line's wording), not introduced here; the fix (the words, or one
// line reserved) is the operator's design call, so it is recorded, not decided.
test.fixme("run page MODEL section: the same size live and finished with the thermal governor armed (owner: the operator's call on ACTIVE TIME's sub line)", async () => {});

// (#2915 review, C7) The machine page's Utility section is ONE size whatever
// the machine's utility jobs are doing: quiet, routing, compacting, a job
// this build has no visual for, a stalled job, and pre-1.61.0 records that
// name no job (folded into the fixed "other" row). Live only: the machine
// page reads the page clock.
const UTIL = { section: ".mm-utility", jobs: ".mm-utility__jobs", strip: ".mm-utility .mach-util" };
for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
  test(`machine page Utility section: one size in every utility state (${vpName})`, async ({ browser }) => {
    const states = STATES.filter((s) => s.utilLive);
    expect(states.map((s) => s.id)).toEqual(expect.arrayContaining(["prompt", "compacting", "radio-routing", "utility-generic", "utility-stalled", "utility-legacy"]));
    const rows = [];
    for (const state of states) {
      const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
      const page = await ctx.newPage();
      await page.clock.setFixedTime(state.nowMs);
      await installLayoutRoutes(page, { machineSpecs: true });
      await page.goto("/index.html#lens=machine");
      await expect(page.locator(".mm-utility__live"), `${state.id}: the live line`).toHaveText(state.utilLive);
      await expect(page.locator(".mm-utility__job")).toHaveCount(3);
      if (state.id === "utility-legacy") await expect(page.locator(".mm-utility__job").nth(2)).toContainText("1 call");
      await page.waitForTimeout(300);
      rows.push({ state: state.id, ...(await measure(page, UTIL)) });
      await ctx.close();
    }
    for (const key of Object.keys(UTIL)) {
      const groups = sizeGroups(rows, key);
      expect(groups, `${key} changed size between utility states (${vpName}):\n  ${groups.join("\n  ")}`).toHaveLength(1);
    }
  });
}
