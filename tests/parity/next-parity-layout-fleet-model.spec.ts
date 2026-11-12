// @ts-nocheck
// (#3554) The fleet card's count line names the model (and engine) behind
// "1 running", and how many models an idle machine holds ("0 running · 2
// loaded"). New words go into the count line's one existing slot: the model's
// name alone (`modelShortName`), and a line still too wide (a hosted run's
// engine on a narrow card) is ellipsized there, never wrapped, so every variant keeps one card
// size (operator: "no layout size changes"), on a desktop and a phone. The full
// line is the slot's title.
//
// Each variant first asserts its words, so a fixture that drifted into another
// state fails here rather than measuring one state several times.
//
// Set DARKMUX_FLEET_MODEL_SHOTS to a directory to also save a screenshot of
// each variant at each width.
const { test, expect } = require("@playwright/test");
const fs = require("fs");
const path = require("path");
const { STATES, MULTI, VIEWPORTS, SELF_ROW, OFFLINE_ROW, installLayoutRoutes, measure } = require("./lib/layout-fixture.js");

// A real id (the parity corpus's resident): the card shows only its name.
const LONG_MODEL = "darkmux:qwen3.6-35b-a3b-turboquant-mlx";
const resident = (identifier) => ({ identifier, model: identifier.replace(/^darkmux:/, ""), status: "idle", size: "1 GB", context: 32768 });

/** A Flint server darkmux started on the fixture machine, answering. */
const FLINT = { endpoint: "local-flint", engine: "flint", model: "flint-int4", served_name: "flint", n_ctx: 32768, port: 8092, pid: 1, started_at_ms: 0, answering: true };

/** `SELF_ROW` with its card's specs changed. */
const selfWith = (specs) => ({ ...SELF_ROW, card: { ...SELF_ROW.card, card: { ...SELF_ROW.card.card, specs: { ...SELF_ROW.card.card.specs, ...specs } } } });

/** `state`'s records with each start record rewritten by `start`. */
const restarted = (state, start) => state.records.map((r) => (r.action === "dispatch.start" ? start(r) : r));

const finished = STATES.find((s) => s.id === "finished");
const generating = STATES.find((s) => s.id === "generating");

const VARIANTS = [
  {
    id: "idle-loaded",
    state: finished,
    self: selfWith({ loaded_models: [resident("darkmux:qwen-layout"), resident("darkmux:util-layout"), resident("someone-elses-model")] }),
    text: "0 running · 2 loaded",
    title: "0 running · 2 loaded",
  },
  {
    id: "idle-unknown",
    state: finished,
    self: selfWith({ lms_unreachable: true }),
    text: "0 running",
    title: /^0 running · loaded models unknown: LM Studio could not be asked/,
  },
  {
    // (#3554, operator) A server darkmux started counts as loaded.
    id: "idle-flint-only",
    state: finished,
    self: selfWith({ loaded_models: [resident("someone-elses-model")], engine_servers: [FLINT] }),
    text: "0 running · 1 loaded",
    title: "0 running · 1 loaded",
  },
  {
    // The engine's list unknown, a server known: a lower bound, never a total.
    id: "idle-unknown-server",
    state: finished,
    self: selfWith({ lms_unreachable: true, engine_servers: [FLINT] }),
    text: "0 running · 1+ loaded",
    title: /^0 running · 1\+ loaded: 1 server darkmux started; the rest is unknown: LM Studio could not be asked/,
  },
  {
    id: "running-local",
    state: generating,
    self: SELF_ROW,
    records: restarted(generating, (r) => ({ ...r, model: LONG_MODEL })),
    text: "1 running · qwen3.6-35b-a3b",
    title: "1 running · qwen3.6-35b-a3b-turboquant-mlx",
  },
  {
    id: "running-hosted",
    state: generating,
    self: SELF_ROW,
    records: restarted(generating, (r) => ({ ...r, model: "gpt-4o", payload: { ...r.payload, endpoint: "azure:myorg-eastus2.openai.azure.com/gpt-4o" } })),
    text: "1 running · gpt-4o · Azure OpenAI",
    title: "1 running · gpt-4o · Azure OpenAI on myorg-eastus2.openai.azure.com",
  },
  {
    id: "several",
    state: MULTI,
    self: SELF_ROW,
    text: "2 running · 1/2 · qwen-layout",
    title: "2 running · 1/2 · qwen-layout",
  },
];

// The machine serving the page; the other card is the offline peer.
const SELF_CARD = ".mach:not(.absent)";

for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
  test(`fleet card: the model and loaded count change no size (${vpName})`, async ({ browser }) => {
    const sizes = [];
    for (const v of VARIANTS) {
      const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
      const page = await ctx.newPage();
      await page.clock.setFixedTime(v.state.nowMs);
      await installLayoutRoutes(page, { fleetView: [v.self, OFFLINE_ROW] });
      // A later route wins: this variant's own records for its state's day.
      if (v.records) await page.route(`**/flow/${v.state.date}`, (route) => route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(v.records) }));
      await page.goto("/index.html#lens=fleet");
      await expect(page.locator(".mach")).toHaveCount(2);
      const runs = page.locator(`${SELF_CARD} .runs`);
      await expect(runs, `${v.id}: the count line`).toHaveText(v.text);
      await expect(runs, `${v.id}: the count line's title`).toHaveAttribute("title", v.title);
      // One line in its slot: never wrapped, ellipsized when it is wider.
      const box = await runs.evaluate((e) => ({ ws: getComputedStyle(e).whiteSpace, to: getComputedStyle(e).textOverflow }));
      expect(box.ws, `${v.id}: the count line does not wrap`).toBe("nowrap");
      expect(box.to, `${v.id}: the count line ellipsizes`).toBe("ellipsis");
      await page.waitForTimeout(400);
      const m = await measure(page, { card: SELF_CARD, runs: `${SELF_CARD} .runs`, tube: `${SELF_CARD} .mach-scope` });
      sizes.push({ id: v.id, servers: v.self.card.card.specs.engine_servers?.length > 0, card: m.card.map((b) => `${b.w}x${b.h}`), runsH: m.runs.map((b) => b.h), tube: m.tube.map((b) => `${b.w}x${b.h}`) });
      if (process.env.DARKMUX_FLEET_MODEL_SHOTS) {
        fs.mkdirSync(process.env.DARKMUX_FLEET_MODEL_SHOTS, { recursive: true });
        // The cards, both of them: a phone stacks them below the fold.
        await page.locator(".mach").last().scrollIntoViewIfNeeded();
        await page.screenshot({ path: path.join(process.env.DARKMUX_FLEET_MODEL_SHOTS, `fleet-${vpName}-${v.id}.png`), fullPage: true });
      }
      await ctx.close();
    }
    const report = sizes.map((s) => `${s.id}: card ${s.card} runs ${s.runsH} tube ${s.tube}`).join("\n  ");
    // The servers line (#3450) is a line of its own that only a machine
    // running a server shows, and on a desktop it adds that line's height:
    // already so before #3554, and not this count line's doing. So card sizes
    // are compared among cards that agree on it; the count line and the tube
    // are compared across every variant.
    for (const servers of [false, true]) {
      const group = sizes.filter((s) => s.servers === servers);
      expect(new Set(group.map((s) => JSON.stringify(s.card))).size, `card sizes differ (${vpName}, servers line ${servers ? "shown" : "absent"}):\n  ${report}`).toBe(1);
    }
    expect(new Set(sizes.map((s) => JSON.stringify(s.runsH))).size, `count line heights differ (${vpName}):\n  ${report}`).toBe(1);
    expect(new Set(sizes.map((s) => JSON.stringify(s.tube))).size, `tube sizes differ (${vpName}):\n  ${report}`).toBe(1);
  });
}
