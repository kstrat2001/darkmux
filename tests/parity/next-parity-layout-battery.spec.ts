// @ts-nocheck
// The machine lens's battery meter keeps ONE size in every charge state
// (operator: "no layout size changes"). `held` adds a marker inside the glyph
// and a "held" label after the percent; the glyph and the row's height must
// not notice. Each state first asserts the words it must show, so a fixture
// that slid into another state fails here rather than measuring one state N times.
//
// Set DARKMUX_BATTERY_SHOTS to a directory to also save a screenshot per
// state and viewport.
const { test, expect } = require("@playwright/test");
const fs = require("fs");
const path = require("path");
const { VIEWPORTS, installLayoutRoutes, measure } = require("./lib/layout-fixture.js");

const RESOURCES = JSON.parse(fs.readFileSync(path.join(__dirname, "corpus", "machine-resources.json"), "utf8"));
const SIZED = { glyph: ".battery-bar", row: ".battery-bar-row", fill: ".battery-bar-fill" };

const STATES = [
  { id: "held", battery: { charge_pct: 80, on_ac: true, charging: false, state: "held", minutes_to_empty: null }, held: true },
  { id: "charging", battery: { charge_pct: 80, on_ac: true, charging: true, state: "charging", minutes_to_empty: null }, held: false },
  { id: "discharging", battery: { charge_pct: 80, on_ac: false, charging: false, state: "discharging", minutes_to_empty: 130 }, held: false },
  { id: "full", battery: { charge_pct: 100, on_ac: true, charging: false, state: "full", minutes_to_empty: null }, held: false },
];

for (const [vpName, viewport] of Object.entries(VIEWPORTS)) {
  test(`machine lens battery meter: glyph and row height hold in every charge state (${vpName})`, async ({ browser }) => {
    const rows = [];
    for (const state of STATES) {
      const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
      const page = await ctx.newPage();
      await installLayoutRoutes(page, { machineSpecs: true });
      const body = { ...RESOURCES, load: { ...RESOURCES.load, now: { ...RESOURCES.load.now, battery: state.battery } } };
      await page.route("**/machine/resources", (route) =>
        route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) }),
      );
      await page.goto("/index.html#lens=machine");
      await expect(page.locator(".battery-bar-pct")).toHaveText(`${state.battery.charge_pct}%`);
      await expect(page.locator(".battery-bar-held"), state.id).toHaveCount(state.held ? 1 : 0);
      await expect(page.locator(".battery-bar-hold"), state.id).toHaveCount(state.held ? 1 : 0);
      await page.waitForTimeout(1600); // the count-up and the fill's ease finish
      const m = await measure(page, SIZED);
      rows.push({ state: state.id, glyph: m.glyph[0], rowH: m.row[0].h, rowW: m.row[0].w });
      if (process.env.DARKMUX_BATTERY_SHOTS) {
        fs.mkdirSync(process.env.DARKMUX_BATTERY_SHOTS, { recursive: true });
        await page.locator(".battery-block").screenshot({ path: path.join(process.env.DARKMUX_BATTERY_SHOTS, `${state.id}-${vpName}.png`) });
      }
      if (state.held) {
        // Opening the explanation must not move the row either: it floats over the page.
        await page.getByRole("button", { name: "what held means" }).focus();
        await expect(page.getByRole("tooltip")).toBeVisible();
        const open = await measure(page, SIZED);
        expect(open.row[0].h, "the row grew when the (i) opened").toBe(m.row[0].h);
        if (process.env.DARKMUX_BATTERY_SHOTS) {
          await page.locator(".battery-bar-row").scrollIntoViewIfNeeded();
          await page.evaluate(() => window.scrollBy(0, 120));
          const r = await page.locator(".battery-bar-row").boundingBox();
          const t = await page.getByRole("tooltip").boundingBox();
          const clip = { x: 0, y: Math.max(0, r.y - 40), width: viewport.width, height: Math.min(viewport.height - Math.max(0, r.y - 40), t.y + t.height - r.y + 80) };
          await page.screenshot({ path: path.join(process.env.DARKMUX_BATTERY_SHOTS, `held-tooltip-${vpName}.png`), clip });
        }
      }
      await ctx.close();
    }
    console.log(`${vpName}: ${JSON.stringify(rows)}`);
    expect(new Set(rows.map((r) => JSON.stringify(r.glyph))).size, `the glyph changed size (${vpName}): ${JSON.stringify(rows)}`).toBe(1);
    expect(new Set(rows.map((r) => r.rowH)).size, `the row changed height (${vpName}): ${JSON.stringify(rows)}`).toBe(1);
  });
}
