// @ts-nocheck
// The fleet hero is ONE height whether or not a part line is showing
// (operator, 2026-09-26: "no new height, and nothing that appears or
// disappears and shifts layout between states").
//
// #2902's part lines ("140 cached" under INPUT, "145 utility" under ALL
// TOKENS) render only when the window's usage records report them. Measured
// before this suite existed (release builds, a scratch DARKMUX_HOME, this
// suite's own fixture), each took a line of its own: the hero grew 18.5px on
// a desktop with utility spend, and 18.5px or 37px on a phone, then shrank
// again when the spend left the window. Each part now rides on a line that is
// always there (`.savc__lbl` / `.savlblwrap` in `ui/src/styles.css`), and the
// hero measures the same as origin/main's in every variant.
//
// Each variant first asserts which part lines it shows, so a fixture that
// stopped reporting cached or utility tokens fails here rather than
// measuring the plain hero four times.
const { test, expect } = require("@playwright/test");
const { HEROES, PLAYBACK_NOW, VIEWPORTS, installLayoutRoutes, measure } = require("./lib/layout-fixture.js");

const HERO = { hero: ".savings", savrow: ".savrow", savlead: ".savlead", savclasses: ".savclasses" };
// The 561-1180px band, where the lead and the chips sit on separate grid rows
// (`.savrow`'s own media query): a part line there cannot hide behind a taller
// neighbor, so it is the strictest width to hold.
const WIDTHS = { ...VIEWPORTS, band: { width: 1000, height: 900 } };

for (const [vpName, viewport] of Object.entries(WIDTHS)) {
  for (const mode of ["live", "playback"]) {
    test(`fleet hero: one height with and without part lines (${vpName}, ${mode})`, async ({ browser }) => {
      const rows = [];
      for (const h of HEROES) {
        const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
        const page = await ctx.newPage();
        await page.clock.setFixedTime(mode === "live" ? h.nowMs : PLAYBACK_NOW);
        await installLayoutRoutes(page);
        await page.goto(`/index.html${mode === "live" ? "#lens=fleet" : `#${h.date}`}`);
        await expect(page.locator(".savnum.ph-shimmer")).toHaveCount(0);
        await expect(page.locator(".savnum")).not.toHaveText("");
        const parts = page.locator(".savpart");
        const want = [...(h.util ? ["utility"] : []), ...(h.cached ? ["cached"] : [])];
        await expect(parts, `${h.id}: the hero must show exactly these part lines`).toHaveCount(want.length);
        for (const w of want) await expect(parts.filter({ hasText: w })).toHaveCount(1);
        await page.waitForTimeout(400);
        rows.push({ id: h.id, ...(await measure(page, HERO)) });
        await ctx.close();
      }
      for (const key of Object.keys(HERO)) {
        // Height only: a part line on a label's line widens that label's
        // block, which is the point; the hero box itself keeps its width.
        const heights = new Map();
        for (const r of rows) {
          const k = JSON.stringify(r[key].map((b) => (key === "hero" || key === "savrow" ? b : b.h)));
          heights.set(k, [...(heights.get(k) ?? []), r.id]);
        }
        const groups = [...heights.entries()].map(([size, ids]) => `${size} <- ${ids.join(", ")}`);
        expect(groups, `${key} changed size with the part lines (${vpName}, ${mode}):\n  ${groups.join("\n  ")}`).toHaveLength(1);
      }
    });
  }
}
