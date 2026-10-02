// @ts-nocheck
// The utility robot on a phone (operator, 2026-10-01): it sits in the card's
// top-right corner, where the round scope leaves room, at the SAME place on
// every card, with or without a scope, and never touches the scope's circle.
// At the end of the name row it moved with the row, and on a phone each
// card's scope is sized to its own text (`tubeFit.ts`), so a card with a long
// live status line got a smaller scope, a wider name row, and a robot in a
// different place from its neighbors.
const { test, expect } = require("@playwright/test");
const { STATES, SELF_ROW, PEER_ROW, OFFLINE_ROW, installLayoutRoutes } = require("./lib/layout-fixture.js");

const PHONES = { "phone-390": { width: 390, height: 844 }, "phone-430": { width: 430, height: 932 } };

for (const [name, viewport] of Object.entries(PHONES)) {
  test(`fleet card: the utility robot sits in the card's top-right corner on every card (${name})`, async ({ browser }) => {
    const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
    const page = await ctx.newPage();
    await page.clock.setFixedTime(STATES.find((s) => s.id === "finished").nowMs);
    await installLayoutRoutes(page, { fleetView: [SELF_ROW, OFFLINE_ROW] });
    await page.goto("/index.html#lens=fleet");
    await expect(page.locator(".mach")).toHaveCount(2);
    await page.waitForTimeout(400);
    const cards = await page.$$eval(".mach", (cs) =>
      cs.map((c) => {
        const r = c.getBoundingClientRect();
        const u = c.querySelector(".mach-util").getBoundingClientRect();
        const t = c.querySelector(".mach-scope")?.getBoundingClientRect() ?? null;
        return {
          util: { dx: Math.round(r.right - u.right), dy: Math.round(u.top - r.top), w: u.width, h: u.height, left: u.left, right: u.right, top: u.top, bottom: u.bottom },
          tube: t && { cx: t.left + t.width / 2, cy: t.top + t.height / 2, r: t.width / 2, right: t.right },
        };
      }),
    );
    // The same corner offset on both cards.
    expect(cards[0].util.dx, `right offsets ${cards.map((c) => c.util.dx)}`).toBe(cards[1].util.dx);
    expect(cards[0].util.dy, `top offsets ${cards.map((c) => c.util.dy)}`).toBe(cards[1].util.dy);
    // In the corner, on the card's content edge: 16px of padding and the 1px
    // border from the right edge (where the tube's right edge sits too), and
    // 14px below the border from the top.
    for (const c of cards) {
      expect(c.util.dx).toBeLessThanOrEqual(17);
      expect(c.util.dy).toBeLessThanOrEqual(15);
    }
    for (const { util, tube } of cards.filter((c) => c.tube)) {
      expect(Math.abs(util.right - tube.right), "the robot's right edge is the tube's").toBeLessThanOrEqual(1);
    }
    // Clear of each card's scope circle: the robot's nearest point is outside it.
    for (const { util, tube } of cards.filter((c) => c.tube)) {
      const nx = Math.max(util.left, Math.min(tube.cx, util.right));
      const ny = Math.max(util.top, Math.min(tube.cy, util.bottom));
      expect(Math.hypot(nx - tube.cx, ny - tube.cy), "robot clears the scope circle").toBeGreaterThan(tube.r);
    }
    await ctx.close();
  });
}

// (rec 4) On a desktop the readout (status line, count line, serves line) is
// centered on the tube's axis, not left-aligned under a centered tube.
test("fleet card: the readout is centered on the tube's axis (desktop)", async ({ browser }) => {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 900 }, timezoneId: "UTC", locale: "en-US" });
  const page = await ctx.newPage();
  await page.clock.setFixedTime(STATES.find((s) => s.id === "finished").nowMs);
  await installLayoutRoutes(page, { fleetView: [SELF_ROW, PEER_ROW] });
  await page.goto("/index.html#lens=fleet");
  await expect(page.locator(".mach")).toHaveCount(2);
  await page.waitForTimeout(400);
  // Each line's TEXT center (not its box): a full-width line's box is
  // centered by construction, so the words' own extent is what is measured.
  const cards = await page.$$eval(".mach", (cs) =>
    cs.map((c) => {
      const t = c.querySelector(".mach-scope").getBoundingClientRect();
      const axis = t.left + t.width / 2;
      const mid = (sel) => {
        if (!c.querySelector(sel).textContent) return 0; // an empty serves line has no words to center
        const range = document.createRange();
        range.selectNodeContents(c.querySelector(sel));
        const r = range.getBoundingClientRect();
        return r.left + r.width / 2 - axis;
      };
      return { stat: mid(".stat"), runs: mid(".runs"), serves: mid(".serves") };
    }),
  );
  for (const c of cards) {
    for (const [line, off] of Object.entries(c)) expect(Math.abs(off), `${line} is ${off}px off the tube's axis`).toBeLessThanOrEqual(1);
  }
  await ctx.close();
});
