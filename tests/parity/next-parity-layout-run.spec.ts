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
const { STATES, MULTI, PLAYBACK_NOW, VIEWPORTS, SELF_ROW, PEER_ROW, ONE_ICON_ROW, NO_ICON_ROW, OFFLINE_ROW, installLayoutRoutes, measure } = require("./lib/layout-fixture.js");

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
// (#2955) The status line: "idle", "dispatch in flight", or, while a model
// runs, the live reading itself (`.stat.mach-scope__rate`). One line in
// every state, so measured across all of them.
const STAT = ".mach .stat";
// (#2955 review) Status lines that overflow their one-line slot on a phone
// and are ellipsized, already on origin/main before #2955 (measured there:
// scrollWidth 203 > clientWidth 184). Owned by the fixme at the end of this
// file; every other state with a reading must fit.
const PHONE_OVERFLOW = new Set(["toolgen-named", "armed-toolgen"]);

/** (#2955 review) How the status line LOOKS: its words' color and weight,
 *  and its dot's color. "disconnected" must look the same on every card. */
async function statLook(page) {
  return page.locator(STAT).first().evaluate((e) => {
    const cs = getComputedStyle(e);
    const probe = document.createElement("span");
    probe.style.color = "var(--dim)";
    e.appendChild(probe);
    const dim = getComputedStyle(probe).color;
    probe.remove();
    const lamp = e.querySelector(".lamp-dot");
    const ls = getComputedStyle(lamp);
    // The lamp's color is its fill, or its stroke when it is a dashed ring.
    const lampColor = lamp.dataset.form === "dashed" ? ls.borderTopColor : ls.backgroundColor;
    return { color: cs.color, weight: cs.fontWeight, lamp: lamp.dataset.form, lampColor, dim };
  });
}

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
  await installLayoutRoutes(page, { blockStream: state.blockStream === true, presence: state.presence });
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
  // Live only. A run page at rest judges its run as of the wall clock
  // (`lib/lifecycle.ts`), so a page pinned weeks after a fixture day reads
  // each unfinished run as stopped with no ending recorded, which is the
  // `finished` state measured here already. A recorded run is replayed at
  // the transport's playhead, which this spec cannot set on a run page.
  for (const mode of ["live"]) {
    test(`run page MODEL section: one size in every state (${vpName}, ${mode})`, async ({ browser }) => {
      const states = STATES;
      const rows = [];
      const noteRows = [];
      for (const state of states) {
        const { ctx, page } = await openState(browser, viewport, state, { mode, surface: "run" });
        const lamps = page.locator(RUN.lamps);
        await expect(lamps, `${state.id}: the page must reach this state before it is measured`).toHaveAttribute("aria-label", state.runText instanceof RegExp ? state.runText : new RegExp(state.runText));
        // (#2950) A state that names its readout line shows exactly it, or
        // (`null`) shows none.
        if (state.noteText === null) await expect(page.locator(NOTE), `${state.id}: no readout line`).toHaveCount(0);
        else if (state.noteText) await expect(page.locator(NOTE), `${state.id}: the readout line`).toHaveText(state.noteText);
        if (state.rateTextPhone) await expectFits(page.locator(NOTE), `${state.id}: run page readout (${vpName}, ${mode})`);
        // (#2963) A tool's file: the line is one line inside its slot, and
        // however long the path, the FILE NAME is on screen (the path trims
        // from the left). On a phone the long path must actually be trimmed,
        // so this state exercises the trim rather than a path that fit.
        if (state.fileName) {
          await expectFits(page.locator(NOTE), `${state.id}: run page readout (${vpName}, ${mode})`);
          const shown = await page.locator(`${NOTE} .modelbox__note-path`).evaluate((box, name) => {
            const bdi = box.querySelector("bdi");
            const text = bdi.firstChild;
            const range = document.createRange();
            range.setStart(text, text.textContent.length - name.length);
            range.setEnd(text, text.textContent.length);
            const r = range.getBoundingClientRect();
            const b = box.getBoundingClientRect();
            const trimmed = box.scrollWidth > box.clientWidth;
            // A trimmed box draws its ellipsis over the start of the box, so
            // the name must clear that too, not just the box's edge.
            const probe = document.createElement("span");
            probe.textContent = "…";
            box.appendChild(probe);
            const ell = trimmed ? probe.getBoundingClientRect().width : 0;
            probe.remove();
            return { inside: r.left >= b.left + ell - 0.5 && r.right <= b.right + 0.5 && r.width > 0, trimmed };
          }, state.fileName);
          expect(shown.inside, `${state.id}: the file name must be on screen (${vpName}, ${mode})`).toBe(true);
          // (#2963) The "…" of a trimmed path sits where an untrimmed path's
          // first character does: right after "write · ", with no extra gap.
          // The ellipsis is drawn just before the first whole character
          // that clears it, so that character's position says where it is.
          const gap = await page.locator(NOTE).evaluate((note) => {
            const act = note.querySelector(".modelbox__note-act").getBoundingClientRect();
            const box = note.querySelector(".modelbox__note-path");
            const b = box.getBoundingClientRect();
            const text = box.querySelector("bdi").firstChild;
            const probe = document.createElement("span");
            probe.textContent = "…";
            box.appendChild(probe);
            const ell = probe.getBoundingClientRect().width;
            probe.remove();
            const trimmed = box.scrollWidth > box.clientWidth;
            const range = document.createRange();
            for (let i = 0; i < text.textContent.length; i++) {
              range.setStart(text, i);
              range.setEnd(text, i + 1);
              const r = range.getBoundingClientRect();
              if (!trimmed) return r.left - act.right;
              if (r.left >= b.left + ell - 0.5) return r.left - ell - act.right;
            }
            return null;
          });
          expect(Math.abs(gap), `${state.id}: the gap between "·" and the path (${vpName}, ${mode}) is ${gap}px`).toBeLessThanOrEqual(1);
          if (state.id === "tool-file-long" && vpName === "phone") expect(shown.trimmed, `${state.id}: the long path is trimmed on a phone`).toBe(true);
          if (state.id === "tool-file") expect(shown.trimmed, `${state.id}: a short path is shown whole (${vpName}, ${mode})`).toBe(false);
        }
        // (#2963) The readout line itself is one line tall in every state
        // that shows one, a tool's file included.
        if ((await page.locator(NOTE).count()) > 0) noteRows.push({ state: state.id, ...(await measure(page, { note: NOTE })) });
        // (#2902 step 5) A long rest counts down in the tube as a compact
        // duration ("23h 53m"), never raw seconds.
        if (state.tubeText) await expect(page.locator(`${RUN.tube} .token-scope-center`), `${state.id}: the tube's countdown`).toHaveText(new RegExp(`^${state.tubeText}`));
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
        expect.arrayContaining(["toolgen-named", "finished", "compacting", "radio-routing", "rest", "rest-turn-delay", "rest-thermal", "rest-pacing", "rest-battery", "rest-episode-limit", "rest-unknown", "armed-generating", "armed-toolgen", "tool-file", "tool-file-long", "tool-file-mixed", "tool-file-unlisted"]),
      );
      // (#2963) The line's own box, only in states that show it: a tool's
      // two-part line (action, then a left-trimming file box) is the same
      // one line, across the same slot, as TOOL GEN's and REST's.
      expect(noteRows.map((r) => r.state)).toEqual(expect.arrayContaining(["toolgen-named", "tool-file", "tool-file-long", "tool-file-mixed", "rest-thermal"]));
      expect(sizeGroups(noteRows, "note"), `the readout line changed height (${vpName}, ${mode})`).toHaveLength(1);
    });

    test(`fleet card: one size across its running states (${vpName}, ${mode})`, async ({ browser }) => {
      const states = STATES.filter((s) => s.fleet !== false && (mode === "live" || s.fleetPlayback !== false));
      const rows = [];
      for (const state of states) {
        const { ctx, page } = await openState(browser, viewport, state, { mode, surface: "fleet" });
        await expect(page.locator(CARD.card).first()).toBeVisible();
        // (#2955 review) A running machine whose page lost the daemon: the
        // plain "disconnected" status line, not a lit reading.
        if (state.fleetStat) {
          await expect(page.locator(STAT).first(), `${state.id}: the status words`).toHaveText(state.fleetStat);
          await expect(page.locator(CARD.card).first(), `${state.id}: a running card, dim dot`).toHaveClass(/\bactive\b.*\bnosignal\b|\bnosignal\b.*\bactive\b/);
        }
        if (state.rateTextPhone) {
          // (#2950) The words the card SHOWS (`innerText` skips the hidden
          // form): with the state on a desktop card, without it on a phone.
          const want = vpName === "phone" ? state.rateTextPhone : state.rateText;
          await expect(page.locator(CARD.rateLine).first(), `${state.id}: the card's visible reason`).toHaveText(want, { useInnerText: true });
          await expectFits(page.locator(CARD.rateLine).first(), `${state.id}: fleet card status line (${vpName}, ${mode})`);
        } else if (state.rateText) {
          await expect(page.locator(CARD.rateLine).first(), `${state.id}: the card must reach this state`).toHaveText(state.rateText instanceof RegExp ? state.rateText : new RegExp(state.rateText));
          // (#2955 review) Every reading fits its one line, not just REST's.
          if (!(vpName === "phone" && PHONE_OVERFLOW.has(state.id))) await expectFits(page.locator(CARD.rateLine).first(), `${state.id}: fleet card status line (${vpName}, ${mode})`);
        } else {
          await expect(page.locator(CARD.rateLine)).toHaveCount(0);
        }
        if (state.tubeText) await expect(page.locator(`${CARD.cardScope} .token-scope-center`).first(), `${state.id}: the card tube's countdown`).toHaveText(new RegExp(`^${state.tubeText}`));
        // (#2915) The utility strip shows the state's job, or is quiet.
        await expect(page.locator(CARD.util).first(), `${state.id}: the utility strip`).toHaveAttribute("data-visual", state.utilVisual ?? "quiet");
        await expect(page.locator(CARD.util).first(), `${state.id}: the strip's stall`).toHaveAttribute("data-stalled", state.utilStalled ? "true" : "false");
        await page.waitForTimeout(400);
        // (#2955) The reading is the status line, not a line under it.
        if (state.rateText) await expect(page.locator(CARD.rateLine).first(), `${state.id}: the reading is the status line`).toHaveClass(/(^|\s)stat(\s|$)/);
        // (#2955 self-review) On the status line, the reading keeps its
        // state's tone and the thinking shimmer: a rule that moved it onto
        // `.stat` once outranked both and painted every state phosphor green.
        if (state.id === "rest-thermal" || state.id === "think") {
          const { got, want } = await page.locator(CARD.rateLine).first().evaluate((e, id) => {
            const probe = document.createElement("span");
            probe.style.color = id === "think" ? "transparent" : "var(--lamp-rest)";
            e.parentElement.appendChild(probe);
            const want = getComputedStyle(probe).color;
            probe.remove();
            return { got: getComputedStyle(e).color, want };
          }, state.id);
          expect(got, `${state.id}: the status line's color is its state's`).toBe(want);
        }
        rows.push({ state: state.id, running: !!state.rateText, look: await statLook(page), ...(await measure(page, { ...CARD, stat: STAT })) });
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
      // (#2915) The strip is one size in EVERY state, idle included.
      expect(sizeGroups(rows, "util"), `the utility strip changed size (${vpName}, ${mode})`).toHaveLength(1);
      // (#2955 review) "disconnected" looks like every other plain status
      // line ("idle"'s words), with the dim dot, never the reading's lit
      // style. Live only: a replay has no connection to lose.
      if (mode === "live") {
        const ns = rows.find((r) => r.state === "no-signal");
        const idle = rows.find((r) => r.state === "finished");
        expect(ns, "the disconnected state was measured").toBeTruthy();
        expect({ color: ns.look.color, weight: ns.look.weight }, "disconnected: the plain status words").toEqual({ color: idle.look.color, weight: idle.look.weight });
        expect({ form: ns.look.lamp, color: ns.look.lampColor }, "disconnected: the dim dashed lamp").toEqual({ form: "dashed", color: ns.look.dim });
      }
      // (#2955, operator 2026-09-27) The card is also the same size idle and
      // running, on a desktop as on a phone: the reading rides the status
      // line instead of a line of its own. Idle states here: a finished run
      // and a mission between model steps ("dispatch in flight", no
      // reading). Two executions are measured below: the tube pages them.
      expect(rows.filter((r) => !r.running).map((r) => r.state)).toEqual(expect.arrayContaining(["finished", "between-steps"]));
      for (const key of ["card", "cardScope", "stat"]) {
        const groups = sizeGroups(rows, key);
        expect(groups, `${key} changed size between states (${vpName}, ${mode}):\n  ${groups.join("\n  ")}`).toHaveLength(1);
      }
    });
  }
}

// (#2958) Before its first data a fleet card says "checking…" (its stat word
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
        // (#2955) Running too, on both widths now: the desktop card no
        // longer grows a line while a model runs.
        ["generating", byId.generating, false],
      ]) {
        const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
        const page = await ctx.newPage();
        await page.clock.setFixedTime(state.nowMs);
        await installLayoutRoutes(page, offline ? { roster: true, holdPresence: hold } : { holdRuns: hold });
        await page.goto("/index.html#lens=fleet");
        const cards = page.locator(CARD.card);
        await expect(cards).toHaveCount(offline ? 2 : 1);
        // (#2955) A generating card's status line is its reading.
        const want = id === "loading" ? ["checking…"] : id === "loaded" ? ["idle"] : [byId.generating.rateText];
        if (offline) want.push(id === "loading" ? "checking…" : "offline");
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
        // the box: "checking…" and "idle" are different widths of one line.
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

// The fleet view's rows draw the same card: a peer the daemon read (hardware
// on the subtitle line) and one it could not read (a typed status on that
// line) are the size of this machine's own card, in every state. The peer
// row carries a grant for the serving machine, which the card must not show; it
// states `serves_radio` itself, which draws "serves radio" on its serves line.
for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
  test(`fleet card: a peer the view read, and one it could not, keep this machine's card size (${vpName})`, async ({ browser }) => {
    const finished = STATES.find((s) => s.id === "finished");
    const heights = [];
    for (const [label, rows, peerSpec] of [
      ["self alone", [SELF_ROW], null],
      ["peer read, with a grant, no beat", [SELF_ROW, PEER_ROW], "Apple M1 Max · 32 GB"],
      ["peer unreachable, not listening", [SELF_ROW, OFFLINE_ROW], "hardware not reported"],
    ]) {
      const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
      const page = await ctx.newPage();
      await page.clock.setFixedTime(finished.nowMs);
      await installLayoutRoutes(page, { fleetView: rows });
      await page.goto("/index.html#lens=fleet");
      await expect(page.locator(CARD.card)).toHaveCount(rows.length);
      await expect(page.locator(".mach .stat").first()).toHaveText("idle");
      if (peerSpec) {
        await expect(page.locator(".mach .spec").nth(1), `${label}: the peer's subtitle`).toHaveText(peerSpec);
        await expect(page.locator(".mach .stat").nth(1), `${label}: the peer's status`).toHaveText(peerSpec === "hardware not reported" ? "offline" : "online");
        await expect(page.locator(".mach").nth(1).locator(".serves"), `${label}: the serves line`).toHaveText(peerSpec === "hardware not reported" ? "" : "serves 12 profiles · radio");
        // The count line below the status says why (the read card's variant).
        if (peerSpec !== "hardware not reported") await expect(page.locator(".mach .runs").nth(1), `${label}: the peer's second line`).toHaveText("not streaming");
        // In words, on both widths (a touch screen has no tooltip to explain an icon).
        if (peerSpec !== "hardware not reported") await expect(page.locator(".mach").nth(1).locator(".serves")).toBeVisible();
      }
      await page.waitForTimeout(400);
      const m = await measure(page, { card: CARD.card, cardScope: CARD.cardScope });
      heights.push(`${label}: ${m.card.map((b) => `${b.w}x${b.h}`).join(", ")}`);
      await ctx.close();
    }
    const distinct = new Set(heights.flatMap((h) => h.split(": ")[1].split(", ").map((wh) => wh.split("x")[1])));
    expect(distinct.size, `card heights differ across states (${vpName}):\n  ${heights.join("\n  ")}`).toBe(1);
  });
}

// The serves line ("serves 12 profiles · radio") changes no card's size: a card
// with both, one with one, one with neither, and one unreadable are the same
// size, and the words show on a desktop and on a phone alike. The name row
// holds identity, the HUB chip and the robot only.
for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
  test(`fleet card: serves both, one and none keep the same card size (${vpName})`, async ({ browser }) => {
    const finished = STATES.find((s) => s.id === "finished");
    const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
    const page = await ctx.newPage();
    await page.clock.setFixedTime(finished.nowMs);
    const rows = [SELF_ROW, PEER_ROW, ONE_ICON_ROW, NO_ICON_ROW, OFFLINE_ROW];
    await installLayoutRoutes(page, { fleetView: rows });
    await page.goto("/index.html#lens=fleet");
    await expect(page.locator(CARD.card)).toHaveCount(rows.length);
    const card = (name) => page.locator(".mach").filter({ has: page.locator(".mach-name", { hasText: new RegExp(`^${name}$`) }) });
    await expect(card("layout-peer").locator(".serves"), "both").toHaveText("serves 12 profiles · radio");
    await expect(card("layout-one-icon").locator(".serves"), "one").toHaveText("serves 1 profile");
    await expect(card("layout-no-icon").locator(".serves"), "none: the line is there, empty").toHaveText("");
    await expect(card("layout-no-icon").locator(".serves")).toHaveCount(1);
    await expect(page.locator(".mach .name .profiles-served, .mach .name .radio-seat"), "the name row holds no serves facts").toHaveCount(0);
    await expect(card("layout-peer").locator(".serves")).toBeVisible();
    // Never clipped: the widest line fits its slot.
    await expectFits(card("layout-peer").locator(".serves"), `serves line (${vpName})`);
    await page.waitForTimeout(400);
    const m = await measure(page, { card: CARD.card });
    const sizes = new Set(m.card.map((b) => `${b.w}x${b.h}`));
    expect(sizes.size, `card sizes differ (${vpName}): ${m.card.map((b) => `${b.w}x${b.h}`).join(", ")}`).toBe(1);
    await ctx.close();
  });
}

// (#2950, found while adding the thermal REST states) With the thermal
// governor armed, a FINISHED run's MODEL section is shorter than the same run
// live: ACTIVE TIME's sub line reads "so far · 0 s thermal rest" while live
// and wraps, and "0 s thermal rest" once finished and does not. Measured on
// this fixture with an armed finished run: 414px live vs 386.6px finished on
// a 1280px desktop, 905.1px vs 842.8px on a phone. On origin/main since #2890
// (the sub line's wording), not introduced here; the fix (the words, or one
// line reserved) is the operator's design call, so it is recorded, not decided.
test.fixme("run page MODEL section: the same size live and finished with the thermal governor armed (owner: the operator's call on ACTIVE TIME's sub line)", async () => {});

// (W3) Two executions: the tube pages them ("2 running · 1/2"), and there is no
// pager row, so the card is the size of one running a single execution, on a
// desktop as on a phone.
async function pagerRows(browser, viewport) {
  const rows = [];
  for (const state of [STATES.find((s) => s.id === "generating"), MULTI]) {
    const { ctx, page } = await openState(browser, viewport, state, { mode: "live", surface: "fleet" });
    await expect(page.locator(CARD.card)).toHaveCount(1);
    await expect(page.locator(".mach-scope__pager"), `${state.id}: no pager row`).toHaveCount(0);
    if (state === MULTI) {
      await expect(page.locator(".runs")).toHaveText("2 running · 1/2");
      const tube = page.locator('.mach-scope[role="button"]');
      await expect(tube).toHaveAttribute("aria-label", /^execution 1 of 2/);
      await tube.click();
      await expect(page.locator(".runs")).toHaveText("2 running · 2/2");
      await expect(tube).toHaveAttribute("aria-label", /^execution 2 of 2/);
      await tube.press("Enter");
      await expect(page.locator(".runs")).toHaveText("2 running · 1/2");
    }
    await page.waitForTimeout(400);
    rows.push({ state: state.id, ...(await measure(page, { card: CARD.card, cardScope: CARD.cardScope, stat: STAT, runs: ".mach .runs", serves: ".mach .serves" })) });
    await ctx.close();
  }
  return rows;
}
for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
  test(`fleet card: the same size with a second execution (${vpName}, live)`, async ({ browser }) => {
    const rows = await pagerRows(browser, viewport);
    for (const key of ["card", "cardScope", "stat", "serves"]) {
      const groups = sizeGroups(rows, key);
      expect(groups, `${key} changed size with a second execution (${vpName}):\n  ${groups.join("\n  ")}`).toHaveLength(1);
    }
  });
}

// One size in EVERY state a machine card can be in, each state alone on its
// page (beside others a desktop grid stretches the row, which would hide a
// card that grew): every run-page state, two executions, a peer read but not
// streaming (serving both, one, nothing), offline, and the "checking…" a card
// says before its first data.
const EMPTY_DAY = Date.parse("2030-01-01T12:00:00Z");
for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
  test(`fleet card: one size in every state, 2+ executions included (${vpName})`, async ({ browser }) => {
    const rows = [];
    const measureCard = async (page, id) => {
      await page.waitForTimeout(400);
      const m = await measure(page, { card: CARD.card });
      expect(m.card, `${id}: exactly one card`).toHaveLength(1);
      rows.push({ state: id, card: m.card[0] });
    };
    for (const state of [...STATES, MULTI]) {
      const { ctx, page } = await openState(browser, viewport, state, { mode: "live", surface: "fleet" });
      await expect(page.locator(CARD.card)).toHaveCount(1);
      await measureCard(page, state.id);
      await ctx.close();
    }
    for (const [id, row, stat] of [
      ["peer: online, not streaming, serves both", PEER_ROW, "online"],
      ["peer: online, not streaming, serves one", ONE_ICON_ROW, "online"],
      ["peer: online, not streaming, serves none", NO_ICON_ROW, "online"],
      ["peer: offline", OFFLINE_ROW, "offline"],
    ]) {
      const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
      const page = await ctx.newPage();
      await page.clock.setFixedTime(EMPTY_DAY);
      await installLayoutRoutes(page, { fleetView: [row] });
      await page.goto("/index.html#lens=fleet");
      await expect(page.locator(CARD.card)).toHaveCount(1);
      await expect(page.locator(".mach .stat"), `${id}: the status`).toHaveText(stat);
      await measureCard(page, id);
      await ctx.close();
    }
    {
      const state = STATES.find((s) => s.id === "finished");
      const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
      const page = await ctx.newPage();
      await page.clock.setFixedTime(state.nowMs);
      await installLayoutRoutes(page, { holdRuns: true });
      await page.goto("/index.html#lens=fleet");
      await expect(page.locator(".mach .stat")).toHaveText("checking…");
      await measureCard(page, "checking…");
      await ctx.close();
    }
    const groups = sizeGroups(rows, "card");
    expect(groups, `a card changed size between states (${vpName}):\n  ${groups.join("\n  ")}`).toHaveLength(1);
  });
}

// (#2955 review) On a phone, "tool gen · write · 18s" does not fit the fleet
// card's status line and is ellipsized (scrollWidth 203 > clientWidth 184 at
// 390px). The same on origin/main before #2955 (its own rate line, same
// width, a size larger); not introduced by moving it onto the status line.
// The fix (a phone form of the words, as REST's reason has) is a design
// call, so it is recorded here, not decided here.
test.fixme("fleet card: the tool gen status line fits on a phone (owner: the operator's call on its phone wording)", async ({ browser }) => {
  for (const id of PHONE_OVERFLOW) {
    const state = STATES.find((s) => s.id === id);
    const { ctx, page } = await openState(browser, VIEWPORTS.phone, state, { mode: "live", surface: "fleet" });
    await expect(page.locator(CARD.rateLine).first(), `${id}: the card must reach this state`).toHaveText(new RegExp(state.rateText));
    await page.waitForTimeout(400);
    await expectFits(page.locator(CARD.rateLine).first(), `${id}: fleet card status line (phone)`);
    await ctx.close();
  }
});

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

// The cards' order is not final until the fleet view answers (the grid keeps
// the cards' boxes, unpainted); nothing below the cards may move when it
// does. The first activity lane's top is the same before and after, however
// the lane got there: a lane that slid in from another place while the page
// settled (a motion that is not a reorder) fails here.
for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
  test(`fleet lanes: the first lane keeps its place when the cards' order goes final (${vpName})`, async ({ browser }) => {
    const finished = STATES.find((s) => s.id === "finished");
    const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
    const page = await ctx.newPage();
    await page.clock.setFixedTime(finished.nowMs);
    let release;
    const held = new Promise((r) => (release = r));
    await installLayoutRoutes(page, { fleetView: [SELF_ROW], holdView: held });
    await page.goto("/index.html#lens=fleet");
    await expect(page.locator(".fleet")).toHaveAttribute("data-order", "pending");
    await expect(page.locator(".lane").first()).toBeVisible();
    const laneTop = () => page.locator(".lane").first().evaluate((e) => Math.round(e.getBoundingClientRect().top * 10) / 10);
    const pending = [];
    for (let i = 0; i < 4; i++) {
      pending.push(await laneTop());
      await page.waitForTimeout(60);
    }
    await expect(page.locator(".fleet"), "still pending while sampled").toHaveAttribute("data-order", "pending");
    release();
    await expect(page.locator(".fleet")).toHaveAttribute("data-order", "final");
    await page.waitForTimeout(500);
    const final = await laneTop();
    expect(new Set([...pending, final]).size, `first lane top over pending ${JSON.stringify(pending)} then final ${final}`).toBe(1);
    await ctx.close();
  });
}
