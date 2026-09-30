// @ts-nocheck
// (#3022) The HUB badge changes no box's size (operator: "no layout size
// changes"). A fleet card, its name row and the machine lens header measure the
// same with the badge as without it, on a phone and a desktop, and the
// operator's own 1470px panel.
//
// Each variant first asserts where the badge is, so a fixture that stopped
// declaring a hub fails here rather than measuring the plain card twice.
//
// Set DARKMUX_HUB_SHOTS to a directory to also save a screenshot of each
// variant at each width.
const { test, expect } = require("@playwright/test");
const fs = require("fs");
const path = require("path");
const { STATES, VIEWPORTS, SELF_ROW, PEER_ROW, installLayoutRoutes, measure } = require("./lib/layout-fixture.js");

const WIDTHS = { ...VIEWPORTS, "desktop-1470": { width: 1882, height: 1000 } };
const BADGE = '[data-testid="hub-badge"]';

/** `row` declaring `mode` in its card. */
const declaring = (row, mode) => ({ ...row, card: { ...row.card, card: { ...row.card.card, fleet_mode: mode } } });

const VARIANTS = [
  { id: "no hub", rows: [declaring(SELF_ROW, "peer"), declaring(PEER_ROW, "peer")], badged: [] },
  { id: "a peer is the hub", rows: [declaring(SELF_ROW, "peer"), declaring(PEER_ROW, "hub")], badged: [1] },
  { id: "this machine is the hub", rows: [declaring(SELF_ROW, "hub"), declaring(PEER_ROW, "peer")], badged: [0] },
];

for (const [vpName, viewport] of Object.entries(WIDTHS)) {
  test(`fleet card: the HUB badge changes no size (${vpName})`, async ({ browser }) => {
    const finished = STATES.find((s) => s.id === "finished");
    const sizes = [];
    for (const v of VARIANTS) {
      const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
      const page = await ctx.newPage();
      await page.clock.setFixedTime(finished.nowMs);
      await installLayoutRoutes(page, { fleetView: v.rows });
      await page.goto("/index.html#lens=fleet");
      await expect(page.locator(".mach")).toHaveCount(2);
      await expect(page.locator(".mach .stat").first()).toHaveText("idle");
      const at = await page.locator(".mach").evaluateAll((cards, sel) => cards.map((c, i) => (c.querySelector(sel) ? i : -1)).filter((i) => i >= 0), BADGE);
      // The cards' order is the view's own; the badge sits on the hub's card.
      expect(at.length, `${v.id}: how many cards carry the badge`).toBe(v.badged.length);
      await page.waitForTimeout(400);
      const m = await measure(page, { card: ".mach", name: ".mach .name", mico: ".mach .mico", badge: BADGE });
      sizes.push({ id: v.id, card: m.card.map((b) => `${b.w}x${b.h}`), name: m.name.map((b) => b.h), badge: m.badge.map((b) => `${b.w}x${b.h}`) });
      if (process.env.DARKMUX_HUB_SHOTS) {
        fs.mkdirSync(process.env.DARKMUX_HUB_SHOTS, { recursive: true });
        await page.screenshot({ path: path.join(process.env.DARKMUX_HUB_SHOTS, `fleet-${vpName}-${v.id.replace(/ /g, "-")}.png`) });
      }
      await ctx.close();
    }
    const cards = new Set(sizes.map((s) => JSON.stringify(s.card)));
    expect(cards.size, `card sizes differ across variants (${vpName}):\n  ${sizes.map((s) => `${s.id}: ${s.card} badge ${s.badge}`).join("\n  ")}`).toBe(1);
    const names = new Set(sizes.map((s) => JSON.stringify(s.name)));
    expect(names.size, `name row heights differ (${vpName}):\n  ${sizes.map((s) => `${s.id}: ${s.name}`).join("\n  ")}`).toBe(1);
  });

  test(`machine lens header: the HUB badge changes no size (${vpName})`, async ({ browser }) => {
    const finished = STATES.find((s) => s.id === "finished");
    const heights = [];
    for (const v of [VARIANTS[0], VARIANTS[2]]) {
      const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
      const page = await ctx.newPage();
      await page.clock.setFixedTime(finished.nowMs);
      await installLayoutRoutes(page, { fleetView: v.rows, machineSpecs: true });
      await page.goto("/index.html#lens=machine");
      await expect(page.locator(".machine-lens__hdr")).toBeVisible();
      await expect(page.locator(`.machine-lens__hdr ${BADGE}`)).toHaveCount(v.badged.length);
      await page.waitForTimeout(400);
      const m = await measure(page, { hdr: ".machine-lens__hdr" });
      heights.push(`${v.id}: ${m.hdr.map((b) => b.h)}`);
      if (process.env.DARKMUX_HUB_SHOTS) {
        await page.screenshot({ path: path.join(process.env.DARKMUX_HUB_SHOTS, `machine-${vpName}-${v.id.replace(/ /g, "-")}.png`) });
      }
      await ctx.close();
    }
    expect(new Set(heights.map((h) => h.split(": ")[1])).size, `header heights differ (${vpName}):\n  ${heights.join("\n  ")}`).toBe(1);
  });
}
