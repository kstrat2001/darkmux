// @ts-nocheck
// The runs board's filter bar adds ONE fixed block of height and moves nothing
// when a filter is chosen (#2925; operator, 2026-09-26: "no new height, and
// nothing that appears or disappears and shifts layout between states").
//
// The bar is one row that scrolls sideways inside itself on a phone, and the
// summary row under it is one line, so the first run row sits at the same y
// with no filter, one value, several values, and a selection that matches
// nothing. A pill may widen to name its selection, but never changes place in
// the order, never changes row, and never moves a pill to its left.
//
// Each state first asserts the words it must show, so a fixture that slid into
// another state fails here rather than measuring one state N times.
const { test, expect } = require("@playwright/test");
const { VIEWPORTS, installLayoutRoutes } = require("./lib/layout-fixture.js");

const NOW = Date.UTC(2026, 9, 1, 12, 0, 0);
const S = NOW / 1000;
const RUNS = [
  { id: "run-a", kind: "dispatch", status: "complete", tracked: true, updated_ts: S - 600, role: "coder", model: "darkmux:qwen", machine: "studio", machine_uid: "AA" },
  { id: "run-b", kind: "dispatch", status: "complete", tracked: true, updated_ts: S - 7200, role: "coder", model: "qwen", machine: "studio-renamed", machine_uid: "aa" },
  { id: "run-c", kind: "mission", status: "running", tracked: true, updated_ts: S - 100, role: "reviewer", model: "llama", machine: "laptop", machine_uid: "BB" },
  { id: "run-d", kind: "lab", status: "complete", tracked: true, updated_ts: S - 3 * 86400, workload: "w1", verify_passed: false, machine: "laptop", machine_uid: "BB" },
];

const BAR = ".fbar";
const SUMMARY = ".fsummary";
const PILLS = ".fpill";
const FIRST_ROW = ".labrunrow";

async function open(browser, viewport) {
  const ctx = await browser.newContext({ viewport, timezoneId: "UTC", locale: "en-US" });
  const page = await ctx.newPage();
  await page.clock.setFixedTime(NOW);
  await installLayoutRoutes(page, { blockStream: true });
  // Registered after the harness's catch-all, so it answers first.
  await page.route("**/runs", (r) => r.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ runs: RUNS, generated_at_ms: NOW }) }));
  await page.route("**/lab/runs", (r) => r.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ configured: true, dir: "/lab", exists: true, runs: [] }) }));
  await page.goto("/index.html#lens=runs");
  await expect(page.locator(BAR)).toBeVisible();
  // Everything above the bar (masthead, status strips) settles before the
  // first measurement, so a later difference is the filter's doing.
  await expect(page.locator(FIRST_ROW).first()).toBeVisible();
  await page.waitForLoadState("networkidle");
  return { ctx, page };
}

async function snapshot(page) {
  return page.evaluate(
    ({ bar, summary, pills, row }) => {
      const box = (el) => {
        const r = el.getBoundingClientRect();
        // Document coordinates: a click may scroll the page, which is not a move.
        return { x: Math.round((r.x + window.scrollX) * 10) / 10, y: Math.round((r.y + window.scrollY) * 10) / 10, w: Math.round(r.width * 10) / 10, h: Math.round(r.height * 10) / 10 };
      };
      return {
        bar: box(document.querySelector(bar)),
        summary: box(document.querySelector(summary)),
        pills: [...document.querySelectorAll(pills)].map((p) => ({ dim: p.dataset.dim, ...box(p) })),
        firstRow: document.querySelector(row) ? box(document.querySelector(row)) : null,
        bodyOverflow: document.body.scrollWidth - document.body.clientWidth,
      };
    },
    { bar: BAR, summary: SUMMARY, pills: PILLS, row: FIRST_ROW },
  );
}

async function choose(page, dim, optionText) {
  await page.locator(`${PILLS}[data-dim="${dim}"]`).click();
  await page.getByRole("dialog").getByLabel(optionText).first().check();
  await page.keyboard.press("Escape");
}

for (const [name, viewport] of Object.entries(VIEWPORTS)) {
  test(`runs filter bar keeps one size and one order while filters are chosen (${name})`, async ({ browser }) => {
    const { ctx, page } = await open(browser, viewport);
    const none = await snapshot(page);
    expect(none.pills.map((p) => p.dim)).toEqual(["time", "status", "machine", "model", "role", "workload", "verify", "route", "tracked"]);
    expect(none.bar.h, "the bar is one fixed row").toBe(34);
    expect(none.summary.h, "the summary is one fixed line").toBe(32);
    expect(none.bodyOverflow, "no horizontal page scroll").toBeLessThanOrEqual(0);

    // One value: Status = running leaves only run-c, so the first row changes,
    // but nothing above it may.
    await choose(page, "status", "running");
    await expect(page.getByText("Showing 1 of 4 runs")).toBeVisible();
    const one = await snapshot(page);
    expect(one.pills.map((p) => p.dim)).toEqual(none.pills.map((p) => p.dim));
    expect(one.bar).toEqual(none.bar);
    expect(one.summary).toEqual(none.summary);
    expect(one.firstRow.y, "the first row stays where it was").toBe(none.firstRow.y);
    expect(one.bodyOverflow).toBeLessThanOrEqual(0);
    for (const p of none.pills.filter((p) => p.x < none.pills.find((q) => q.dim === "status").x)) {
      expect(one.pills.find((q) => q.dim === p.dim).x, `${p.dim} pill is left of the choice and must not move`).toBe(p.x);
    }

    // Two filters that together match nothing (reached by link: the popover
    // lists only values that still match something, so it cannot get here).
    await page.evaluate(() => {
      location.hash = "lens=runs&role=coder&status=running";
    });
    await expect(page.getByText(/no runs match these filters/)).toBeVisible();
    const empty = await snapshot(page);
    expect(empty.pills.map((p) => p.dim)).toEqual(none.pills.map((p) => p.dim));
    expect(empty.bar).toEqual(none.bar);
    expect(empty.summary).toEqual(none.summary);
    expect(empty.bodyOverflow).toBeLessThanOrEqual(0);

    // Clear all returns the board to the shape it opened with.
    await page.getByRole("button", { name: "Clear all" }).click();
    expect(await snapshot(page)).toEqual(none);
    await ctx.close();
  });
}

test("on a phone the popover is a bottom sheet", async ({ browser }) => {
  const { ctx, page } = await open(browser, VIEWPORTS.phone);
  await page.locator(`${PILLS}[data-dim="role"]`).click();
  const box = await page.getByRole("dialog").boundingBox();
  expect(box.x).toBe(0);
  expect(box.width).toBe(VIEWPORTS.phone.width);
  expect(Math.round(box.y + box.height)).toBe(VIEWPORTS.phone.height);
  await ctx.close();
});

// (#2925) The kind tabs are one row like the filter bar: on a screen too narrow
// for all four, the row scrolls sideways instead of dropping a tab to a line of
// its own (operator, 2026-10-01: the lab tab took a whole line).
test("on a narrow phone the kind tabs stay on one line and scroll", async ({ browser }) => {
  const { ctx, page } = await open(browser, { width: 320, height: VIEWPORTS.phone.height });
  const layout = await page.evaluate(() => {
    const bar = document.querySelector(".runsbar");
    const chips = [...bar.querySelectorAll(".runchip")].map((c) => c.getBoundingClientRect());
    return {
      tops: [...new Set(chips.map((r) => Math.round(r.top)))],
      barHeight: Math.round(bar.getBoundingClientRect().height),
      chipHeight: Math.round(chips[0].height),
      scrolls: bar.scrollWidth > bar.clientWidth,
      barRight: Math.round(bar.getBoundingClientRect().right),
      viewport: window.innerWidth,
    };
  });
  expect(layout.tops).toHaveLength(1);
  expect(layout.barHeight).toBe(layout.chipHeight);
  expect(layout.scrolls).toBe(true);
  expect(layout.barRight).toBeLessThanOrEqual(layout.viewport);
  await ctx.close();
});
